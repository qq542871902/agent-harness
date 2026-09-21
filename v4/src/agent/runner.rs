use anyhow::{Context, Result};
use tracing::info;

use crate::{
    llm::{ChatRequest, LlmClient, Message, ToolCall},
    policy::{ApprovalDecision, ApprovalHandler, ApprovalState, Policy, ToolPermission},
    tools::{ToolOutput, ToolRegistry},
};

use super::{AgentState, AgentStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    Completed { content: String },
    MaxStepsReached { steps: usize },
}

/// Drives bounded, policy-checked model → tool → observation turns for one task.
pub struct AgentRunner<'a> {
    client: &'a dyn LlmClient,
    tools: &'a ToolRegistry,
    model: &'a str,
    policy: &'a dyn Policy,
    approver: &'a dyn ApprovalHandler,
}

impl<'a> AgentRunner<'a> {
    pub fn new(
        client: &'a dyn LlmClient,
        tools: &'a ToolRegistry,
        model: &'a str,
        policy: &'a dyn Policy,
        approver: &'a dyn ApprovalHandler,
    ) -> Self {
        Self {
            client,
            tools,
            model,
            policy,
            approver,
        }
    }

    pub async fn run(&self, state: &mut AgentState) -> Result<AgentOutcome> {
        state.status = AgentStatus::Running;
        let mut approvals = ApprovalState::default();

        while state.step < state.max_steps {
            state.step += 1;
            info!(step = state.step, "sending agent model request");

            let request = ChatRequest::from_messages(self.model, state.messages.clone())
                .with_tools(self.tools.definitions());
            let response = match self.client.chat(request).await {
                Ok(response) => response,
                Err(error) => {
                    state.status = AgentStatus::Failed;
                    return Err(error);
                }
            };

            if response.tool_calls.is_empty() {
                let content = match response
                    .content
                    .filter(|content| !content.trim().is_empty())
                    .context("model returned a final response without content")
                {
                    Ok(content) => content,
                    Err(error) => {
                        state.status = AgentStatus::Failed;
                        return Err(error);
                    }
                };
                state.messages.push(Message::assistant(content.clone()));
                state.status = AgentStatus::Completed;
                return Ok(AgentOutcome::Completed { content });
            }

            let calls = response.tool_calls;
            let assistant_message = match Message::assistant_tool_calls(response.content, &calls) {
                Ok(message) => message,
                Err(error) => {
                    state.status = AgentStatus::Failed;
                    return Err(error);
                }
            };
            state.messages.push(assistant_message);

            for call in calls {
                let observation = self
                    .policy_checked_observation(state, &mut approvals, &call)
                    .await;
                state
                    .messages
                    .push(Message::tool_result(call.id, observation));
            }
        }

        state.status = AgentStatus::MaxStepsReached;
        Ok(AgentOutcome::MaxStepsReached { steps: state.step })
    }

    async fn policy_checked_observation(
        &self,
        state: &mut AgentState,
        approvals: &mut ApprovalState,
        call: &ToolCall,
    ) -> String {
        match self.policy.permission(call) {
            ToolPermission::Deny => {
                format!(
                    "Tool call denied by policy: `{}` is not permitted",
                    call.name
                )
            }
            ToolPermission::Allow => self.execute_observation(call).await,
            ToolPermission::Ask if approvals.is_always_allowed(&call.name) => {
                self.execute_observation(call).await
            }
            ToolPermission::Ask => {
                state.status = AgentStatus::WaitingApproval;
                let decision = self.approver.request_approval(call);
                state.status = AgentStatus::Running;

                match decision {
                    Ok(ApprovalDecision::GrantOnce) => self.execute_observation(call).await,
                    Ok(ApprovalDecision::AlwaysAllow) => {
                        approvals.always_allow(call.name.clone());
                        self.execute_observation(call).await
                    }
                    Ok(ApprovalDecision::Deny) => {
                        format!(
                            "Tool call rejected by user approval: `{}` was not executed",
                            call.name
                        )
                    }
                    Err(error) => format!(
                        "Tool call denied because approval was unavailable: `{}` was not executed\n{error:#}",
                        call.name
                    ),
                }
            }
        }
    }

    async fn execute_observation(&self, call: &ToolCall) -> String {
        match self.tools.execute(call).await {
            Ok(ToolOutput {
                content,
                success: true,
            }) => content,
            Ok(ToolOutput {
                content,
                success: false,
            }) => format!("Tool execution failed:\n{content}"),
            Err(error) => format!("Tool execution failed:\n{error:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        llm::{ModelResponse, Role},
        policy::{DefaultPolicy, ToolPermission},
        test_support::TestWorkspace,
        tools::{ReadFileTool, Tool},
    };

    struct FakeClient {
        responses: Mutex<VecDeque<ModelResponse>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait]
    impl LlmClient for FakeClient {
        async fn chat(&self, request: ChatRequest) -> Result<ModelResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow!("fake client ran out of responses"))
        }
    }

    struct FakeApprover {
        decisions: Mutex<VecDeque<ApprovalDecision>>,
        requests: AtomicUsize,
    }

    impl FakeApprover {
        fn new(decisions: impl IntoIterator<Item = ApprovalDecision>) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().collect()),
                requests: AtomicUsize::new(0),
            }
        }
    }

    impl ApprovalHandler for FakeApprover {
        fn request_approval(&self, _call: &ToolCall) -> Result<ApprovalDecision> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            self.decisions
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow!("fake approver ran out of decisions"))
        }
    }

    struct FixedPolicy(ToolPermission);

    impl Policy for FixedPolicy {
        fn permission(&self, _call: &ToolCall) -> ToolPermission {
            self.0
        }
    }

    struct SequencedPolicy(Mutex<VecDeque<ToolPermission>>);

    impl Policy for SequencedPolicy {
        fn permission(&self, _call: &ToolCall) -> ToolPermission {
            self.0.lock().unwrap().pop_front().unwrap()
        }
    }

    struct CountingTool {
        executions: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &'static str {
            "count"
        }

        fn description(&self) -> &'static str {
            "counts executions"
        }

        fn schema(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: "executed".into(),
                success: true,
            })
        }
    }

    fn client_with_calls(calls: Vec<ToolCall>) -> FakeClient {
        FakeClient {
            responses: Mutex::new(VecDeque::from([
                ModelResponse {
                    content: None,
                    tool_calls: calls,
                },
                ModelResponse {
                    content: Some("done".into()),
                    tool_calls: vec![],
                },
            ])),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn counting_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
        let executions = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools
            .register(CountingTool {
                executions: Arc::clone(&executions),
            })
            .unwrap();
        (tools, executions)
    }

    fn count_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "count".into(),
            arguments: json!({}),
        }
    }

    #[tokio::test]
    async fn multi_turn_flow_replays_tool_observation() {
        let workspace = TestWorkspace::new("runner");
        fs::write(workspace.path().join("note.txt"), "inspected").unwrap();
        let mut tools = ToolRegistry::new();
        tools
            .register(ReadFileTool::new(workspace.path()).unwrap())
            .unwrap();
        let client = client_with_calls(vec![ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "note.txt"}),
        }]);
        let approver = FakeApprover::new([]);
        let runner = AgentRunner::new(&client, &tools, "test-model", &DefaultPolicy, &approver);
        let mut state = AgentState::new("inspect first");

        let outcome = runner.run(&mut state).await.unwrap();

        assert_eq!(
            outcome,
            AgentOutcome::Completed {
                content: "done".into()
            }
        );
        assert_eq!(state.status, AgentStatus::Completed);
        assert_eq!(state.step, 2);
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages[0].role, Role::System);
        assert_eq!(requests[1].messages.len(), 4);
        assert_eq!(requests[1].messages[3].role, Role::Tool);
        assert_eq!(
            requests[1].messages[3].tool_call_id.as_deref(),
            Some("call-1")
        );
        assert_eq!(
            requests[1].messages[3].content.as_deref(),
            Some("inspected")
        );
    }

    #[tokio::test]
    async fn denied_observation_reaches_second_model_turn() {
        let tools = ToolRegistry::new();
        let client = client_with_calls(vec![ToolCall {
            id: "missing".into(),
            name: "unknown".into(),
            arguments: json!({}),
        }]);
        let approver = FakeApprover::new([]);
        let runner = AgentRunner::new(&client, &tools, "test-model", &DefaultPolicy, &approver);
        let mut state = AgentState::new("task");

        runner.run(&mut state).await.unwrap();

        let requests = client.requests.lock().unwrap();
        let observation = &requests[1].messages[3];
        assert_eq!(observation.tool_call_id.as_deref(), Some("missing"));
        assert!(
            observation
                .content
                .as_deref()
                .unwrap()
                .contains("denied by policy")
        );
        assert_eq!(approver.requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn grant_deny_and_always_decisions_control_execution() {
        for (decision, expected_executions) in [
            (ApprovalDecision::GrantOnce, 1),
            (ApprovalDecision::Deny, 0),
        ] {
            let (tools, executions) = counting_registry();
            let client = client_with_calls(vec![count_call("one")]);
            let approver = FakeApprover::new([decision]);
            let runner = AgentRunner::new(
                &client,
                &tools,
                "test-model",
                &FixedPolicy(ToolPermission::Ask),
                &approver,
            );
            runner.run(&mut AgentState::new("task")).await.unwrap();
            assert_eq!(executions.load(Ordering::SeqCst), expected_executions);
            assert_eq!(approver.requests.load(Ordering::SeqCst), 1);
        }

        let (tools, executions) = counting_registry();
        let client = client_with_calls(vec![count_call("one"), count_call("two")]);
        let approver = FakeApprover::new([ApprovalDecision::AlwaysAllow]);
        let runner = AgentRunner::new(
            &client,
            &tools,
            "test-model",
            &FixedPolicy(ToolPermission::Ask),
            &approver,
        );
        runner.run(&mut AgentState::new("task")).await.unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(approver.requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn policy_runs_before_execution_and_deny_beats_always_allow() {
        let (tools, executions) = counting_registry();
        let client = client_with_calls(vec![count_call("one"), count_call("two")]);
        let policy = SequencedPolicy(Mutex::new(VecDeque::from([
            ToolPermission::Ask,
            ToolPermission::Deny,
        ])));
        let approver = FakeApprover::new([ApprovalDecision::AlwaysAllow]);
        let runner = AgentRunner::new(&client, &tools, "test-model", &policy, &approver);

        runner.run(&mut AgentState::new("task")).await.unwrap();

        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(approver.requests.load(Ordering::SeqCst), 1);
        let requests = client.requests.lock().unwrap();
        assert!(
            requests[1].messages[4]
                .content
                .as_deref()
                .unwrap()
                .contains("denied by policy")
        );
    }
}
