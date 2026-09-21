use anyhow::{Context, Result};
use tracing::info;
use uuid::Uuid;

use crate::{
    llm::{ChatRequest, LlmClient, Message, ToolCall},
    policy::{ApprovalDecision, ApprovalHandler, ApprovalState, Policy, ToolPermission},
    session::SessionSink,
    tools::{ToolOutput, ToolRegistry},
    trace::{TraceEvent, TraceEventKind, TraceSink, safe_error},
};
#[cfg(test)]
use crate::{session::NULL_SESSION_SINK, trace::NULL_TRACE_SINK};

use super::{AgentState, AgentStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    Completed { content: String },
    MaxStepsReached { steps: usize },
}

pub struct RunPersistence<'a> {
    pub session_id: Uuid,
    pub trace: &'a dyn TraceSink,
    pub sessions: &'a dyn SessionSink,
}

/// Drives bounded, policy-checked model → tool → observation turns for one task.
pub struct AgentRunner<'a> {
    client: &'a dyn LlmClient,
    tools: &'a ToolRegistry,
    model: &'a str,
    policy: &'a dyn Policy,
    approver: &'a dyn ApprovalHandler,
    session_id: Uuid,
    trace: &'a dyn TraceSink,
    sessions: &'a dyn SessionSink,
}

impl<'a> AgentRunner<'a> {
    /// V4-compatible composition for tests that do not persist traces.
    #[cfg(test)]
    pub fn new(
        client: &'a dyn LlmClient,
        tools: &'a ToolRegistry,
        model: &'a str,
        policy: &'a dyn Policy,
        approver: &'a dyn ApprovalHandler,
    ) -> Self {
        Self::with_trace(
            client,
            tools,
            model,
            policy,
            approver,
            Uuid::nil(),
            &NULL_TRACE_SINK,
        )
    }

    #[cfg(test)]
    pub fn with_trace(
        client: &'a dyn LlmClient,
        tools: &'a ToolRegistry,
        model: &'a str,
        policy: &'a dyn Policy,
        approver: &'a dyn ApprovalHandler,
        session_id: Uuid,
        trace: &'a dyn TraceSink,
    ) -> Self {
        Self::with_persistence(
            client,
            tools,
            model,
            policy,
            approver,
            RunPersistence {
                session_id,
                trace,
                sessions: &NULL_SESSION_SINK,
            },
        )
    }

    pub fn with_persistence(
        client: &'a dyn LlmClient,
        tools: &'a ToolRegistry,
        model: &'a str,
        policy: &'a dyn Policy,
        approver: &'a dyn ApprovalHandler,
        persistence: RunPersistence<'a>,
    ) -> Self {
        Self {
            client,
            tools,
            model,
            policy,
            approver,
            session_id: persistence.session_id,
            trace: persistence.trace,
            sessions: persistence.sessions,
        }
    }

    pub async fn run(&self, state: &mut AgentState) -> Result<AgentOutcome> {
        self.run_loop(state, false).await
    }

    pub async fn resume(&self, state: &mut AgentState) -> Result<AgentOutcome> {
        self.run_loop(state, true).await
    }

    pub fn abort(&self, state: &mut AgentState) -> Result<()> {
        state.status = AgentStatus::Aborted;
        self.checkpoint(state)?;
        self.emit(TraceEventKind::AgentAborted { step: state.step })
    }

    async fn run_loop(&self, state: &mut AgentState, resumed: bool) -> Result<AgentOutcome> {
        state.status = AgentStatus::Running;
        if let Err(error) = self.checkpoint(state) {
            return self.fail_checkpoint(state, error);
        }
        let mut approvals = ApprovalState::default();
        let event = if resumed {
            TraceEventKind::SessionResumed
        } else {
            TraceEventKind::SessionStarted
        };
        if let Err(error) = self.emit(event) {
            return self.fail(state, error);
        }

        if resumed && let Err(error) = self.recover_in_flight(state) {
            return self.fail(state, error);
        }
        if let Err(error) = self.process_pending_calls(state, &mut approvals).await {
            return self.fail(state, error);
        }

        while state.step < state.max_steps {
            state.step += 1;
            if let Err(error) = self.checkpoint(state) {
                return self.fail_checkpoint(state, error);
            }
            info!(step = state.step, "sending agent model request");

            let request = ChatRequest::from_messages(self.model, state.messages.clone())
                .with_tools(self.tools.definitions());
            if let Err(error) = self.emit(TraceEventKind::ModelRequest { step: state.step }) {
                return self.fail(state, error);
            }
            let response = match self.client.chat(request).await {
                Ok(response) => response,
                Err(error) => return self.fail(state, error),
            };
            if let Err(error) = self.emit(TraceEventKind::ModelResponse {
                step: state.step,
                final_response: response.tool_calls.is_empty(),
                tool_call_count: response.tool_calls.len(),
            }) {
                return self.fail(state, error);
            }

            if response.tool_calls.is_empty() {
                let content = match response
                    .content
                    .filter(|content| !content.trim().is_empty())
                    .context("model returned a final response without content")
                {
                    Ok(content) => content,
                    Err(error) => return self.fail(state, error),
                };
                state.messages.push(Message::assistant(content.clone()));
                state.status = AgentStatus::Completed;
                if let Err(error) = self.checkpoint(state) {
                    return self.fail_checkpoint(state, error);
                }
                if let Err(error) = self.emit(TraceEventKind::AgentCompleted { step: state.step }) {
                    return self.fail(state, error);
                }
                return Ok(AgentOutcome::Completed { content });
            }

            let calls = response.tool_calls;
            let assistant_message = match Message::assistant_tool_calls(response.content, &calls) {
                Ok(message) => message,
                Err(error) => return self.fail(state, error),
            };
            state.messages.push(assistant_message);
            state.pending_tool_calls = calls;
            if let Err(error) = self.checkpoint(state) {
                return self.fail_checkpoint(state, error);
            }
            if let Err(error) = self.process_pending_calls(state, &mut approvals).await {
                return self.fail(state, error);
            }
        }

        state.status = AgentStatus::MaxStepsReached;
        if let Err(error) = self.checkpoint(state) {
            return self.fail_checkpoint(state, error);
        }
        if let Err(error) = self.emit(TraceEventKind::MaxStepsReached { step: state.step }) {
            return self.fail(state, error);
        }
        Ok(AgentOutcome::MaxStepsReached { steps: state.step })
    }

    fn recover_in_flight(&self, state: &mut AgentState) -> Result<()> {
        let Some(call) = state.in_flight_tool_call.clone() else {
            return Ok(());
        };
        let observation = format!(
            "Tool execution outcome is indeterminate after interruption; `{}` was not replayed. Submit a new tool call after reconciling external state.",
            call.name
        );
        state
            .messages
            .push(Message::tool_result(call.id.clone(), observation));
        if let Some(index) = state
            .pending_tool_calls
            .iter()
            .position(|pending| pending.id == call.id)
        {
            state.pending_tool_calls.remove(index);
        }
        state.in_flight_tool_call = None;
        self.checkpoint(state)?;
        self.emit(TraceEventKind::ToolOutcomeIndeterminate {
            id: call.id,
            name: call.name,
        })
    }

    async fn process_pending_calls(
        &self,
        state: &mut AgentState,
        approvals: &mut ApprovalState,
    ) -> Result<()> {
        while let Some(call) = state.pending_tool_calls.first().cloned() {
            self.emit(TraceEventKind::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
            })?;
            let (observation, success) = self
                .policy_checked_observation(state, approvals, &call)
                .await?;
            state
                .messages
                .push(Message::tool_result(call.id.clone(), observation));
            state.pending_tool_calls.remove(0);
            state.in_flight_tool_call = None;
            self.checkpoint(state)?;
            self.emit(TraceEventKind::ToolResult {
                id: call.id,
                name: call.name,
                success,
            })?;
        }
        Ok(())
    }

    async fn policy_checked_observation(
        &self,
        state: &mut AgentState,
        approvals: &mut ApprovalState,
        call: &ToolCall,
    ) -> Result<(String, bool)> {
        let result = match self.policy.permission(call) {
            ToolPermission::Deny => (
                format!(
                    "Tool call denied by policy: `{}` is not permitted",
                    call.name
                ),
                false,
            ),
            ToolPermission::Allow => self.execute_observation(state, call).await?,
            ToolPermission::Ask if approvals.is_always_allowed(&call.name) => {
                self.execute_observation(state, call).await?
            }
            ToolPermission::Ask => {
                self.emit(TraceEventKind::ApprovalRequested {
                    id: call.id.clone(),
                    name: call.name.clone(),
                })?;
                state.status = AgentStatus::WaitingApproval;
                self.checkpoint(state)?;
                let decision = self.approver.request_approval(call);
                state.status = AgentStatus::Running;
                self.checkpoint(state)?;

                match decision {
                    Ok(ApprovalDecision::GrantOnce) => {
                        self.emit(TraceEventKind::ApprovalGranted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        })?;
                        self.execute_observation(state, call).await?
                    }
                    Ok(ApprovalDecision::AlwaysAllow) => {
                        self.emit(TraceEventKind::ApprovalGranted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        })?;
                        approvals.always_allow(call.name.clone());
                        self.execute_observation(state, call).await?
                    }
                    Ok(ApprovalDecision::Deny) => {
                        self.emit(TraceEventKind::ApprovalDenied {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        })?;
                        (
                            format!(
                                "Tool call rejected by user approval: `{}` was not executed",
                                call.name
                            ),
                            false,
                        )
                    }
                    Err(error) => {
                        self.emit(TraceEventKind::ApprovalDenied {
                            id: call.id.clone(),
                            name: call.name.clone(),
                        })?;
                        (
                            format!(
                                "Tool call denied because approval was unavailable: `{}` was not executed\n{error:#}",
                                call.name
                            ),
                            false,
                        )
                    }
                }
            }
        };
        Ok(result)
    }

    async fn execute_observation(
        &self,
        state: &mut AgentState,
        call: &ToolCall,
    ) -> Result<(String, bool)> {
        state.in_flight_tool_call = Some(call.clone());
        self.checkpoint(state)?;
        Ok(match self.tools.execute(call).await {
            Ok(ToolOutput {
                content,
                success: true,
            }) => (content, true),
            Ok(ToolOutput {
                content,
                success: false,
            }) => (format!("Tool execution failed:\n{content}"), false),
            Err(error) => (format!("Tool execution failed:\n{error:#}"), false),
        })
    }

    fn emit(&self, kind: TraceEventKind) -> Result<()> {
        self.trace.record(&TraceEvent::new(self.session_id, kind))
    }

    fn checkpoint(&self, state: &AgentState) -> Result<()> {
        self.sessions.checkpoint(state)
    }

    fn fail<T>(&self, state: &mut AgentState, error: anyhow::Error) -> Result<T> {
        state.status = AgentStatus::Failed;
        let checkpoint_error = self.checkpoint(state).err();
        let diagnostic = checkpoint_error.as_ref().map_or_else(
            || format!("{error:#}"),
            |checkpoint| format!("{error:#}\nsession checkpoint failed: {checkpoint:#}"),
        );
        let _ = self.emit(TraceEventKind::AgentFailed {
            step: state.step,
            error: safe_error(&diagnostic),
        });
        if let Some(checkpoint) = checkpoint_error {
            return Err(checkpoint).context("failed to persist terminal agent state");
        }
        Err(error)
    }

    fn fail_checkpoint<T>(&self, state: &mut AgentState, error: anyhow::Error) -> Result<T> {
        state.status = AgentStatus::Failed;
        let _ = self.emit(TraceEventKind::AgentFailed {
            step: state.step,
            error: safe_error(&format!("session checkpoint failed: {error:#}")),
        });
        Err(error).context("failed to persist agent state")
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

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<TraceEventKind>>);

    impl RecordingSink {
        fn events(&self) -> Vec<TraceEventKind> {
            self.0.lock().unwrap().clone()
        }
    }

    impl TraceSink for RecordingSink {
        fn record(&self, event: &TraceEvent) -> Result<()> {
            self.0.lock().unwrap().push(event.kind.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSessionSink(Mutex<Vec<AgentState>>);

    impl RecordingSessionSink {
        fn states(&self) -> Vec<AgentState> {
            self.0.lock().unwrap().clone()
        }
    }

    impl SessionSink for RecordingSessionSink {
        fn checkpoint(&self, state: &AgentState) -> Result<()> {
            self.0.lock().unwrap().push(state.clone());
            Ok(())
        }
    }

    struct FailingSessionSink;

    impl SessionSink for FailingSessionSink {
        fn checkpoint(&self, _state: &AgentState) -> Result<()> {
            Err(anyhow!("checkpoint unavailable"))
        }
    }

    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &'static str {
            "fail"
        }

        fn description(&self) -> &'static str {
            "returns a recoverable failure"
        }

        fn schema(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "expected failure".into(),
                success: false,
            })
        }
    }

    fn fail_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "fail".into(),
            arguments: json!({"authorization": "super-secret"}),
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

    #[tokio::test]
    async fn final_only_flow_has_deterministic_event_order() {
        let client = FakeClient {
            responses: Mutex::new(VecDeque::from([ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
            }])),
            requests: Mutex::new(Vec::new()),
        };
        let tools = ToolRegistry::new();
        let approver = FakeApprover::new([]);
        let trace = RecordingSink::default();
        let runner = AgentRunner::with_trace(
            &client,
            &tools,
            "test-model",
            &DefaultPolicy,
            &approver,
            Uuid::from_u128(1),
            &trace,
        );

        runner.run(&mut AgentState::new("task")).await.unwrap();

        assert_eq!(
            trace.events(),
            vec![
                TraceEventKind::SessionStarted,
                TraceEventKind::ModelRequest { step: 1 },
                TraceEventKind::ModelResponse {
                    step: 1,
                    final_response: true,
                    tool_call_count: 0,
                },
                TraceEventKind::AgentCompleted { step: 1 },
            ]
        );
    }

    #[tokio::test]
    async fn approved_tool_flow_has_deterministic_event_order() {
        let (tools, _) = counting_registry();
        let client = client_with_calls(vec![count_call("one")]);
        let approver = FakeApprover::new([ApprovalDecision::GrantOnce]);
        let policy = FixedPolicy(ToolPermission::Ask);
        let trace = RecordingSink::default();
        let runner = AgentRunner::with_trace(
            &client,
            &tools,
            "test-model",
            &policy,
            &approver,
            Uuid::from_u128(2),
            &trace,
        );

        runner.run(&mut AgentState::new("task")).await.unwrap();

        assert_eq!(
            trace.events(),
            vec![
                TraceEventKind::SessionStarted,
                TraceEventKind::ModelRequest { step: 1 },
                TraceEventKind::ModelResponse {
                    step: 1,
                    final_response: false,
                    tool_call_count: 1,
                },
                TraceEventKind::ToolCall {
                    id: "one".into(),
                    name: "count".into(),
                },
                TraceEventKind::ApprovalRequested {
                    id: "one".into(),
                    name: "count".into(),
                },
                TraceEventKind::ApprovalGranted {
                    id: "one".into(),
                    name: "count".into(),
                },
                TraceEventKind::ToolResult {
                    id: "one".into(),
                    name: "count".into(),
                    success: true,
                },
                TraceEventKind::ModelRequest { step: 2 },
                TraceEventKind::ModelResponse {
                    step: 2,
                    final_response: true,
                    tool_call_count: 0,
                },
                TraceEventKind::AgentCompleted { step: 2 },
            ]
        );
    }

    #[tokio::test]
    async fn denial_and_tool_failure_are_unsuccessful_results_and_observations() {
        let mut tools = ToolRegistry::new();
        tools.register(FailingTool).unwrap();
        let client = client_with_calls(vec![fail_call("denied"), fail_call("failed")]);
        let policy = SequencedPolicy(Mutex::new(VecDeque::from([
            ToolPermission::Ask,
            ToolPermission::Allow,
        ])));
        let approver = FakeApprover::new([ApprovalDecision::Deny]);
        let trace = RecordingSink::default();
        let runner = AgentRunner::with_trace(
            &client,
            &tools,
            "test-model",
            &policy,
            &approver,
            Uuid::from_u128(3),
            &trace,
        );

        runner.run(&mut AgentState::new("task")).await.unwrap();

        let events = trace.events();
        assert!(events.contains(&TraceEventKind::ApprovalDenied {
            id: "denied".into(),
            name: "fail".into(),
        }));
        let results: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, TraceEventKind::ToolResult { .. }))
            .collect();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|event| matches!(event, TraceEventKind::ToolResult { success: false, .. }))
        );
        let requests = client.requests.lock().unwrap();
        assert!(
            requests[1].messages[3]
                .content
                .as_deref()
                .unwrap()
                .contains("rejected by user")
        );
        assert!(
            requests[1].messages[4]
                .content
                .as_deref()
                .unwrap()
                .contains("Tool execution failed")
        );
    }

    #[tokio::test]
    async fn max_steps_and_fatal_failures_have_terminal_events() {
        let tools = ToolRegistry::new();
        let approver = FakeApprover::new([]);
        let max_client = client_with_calls(vec![ToolCall {
            id: "unknown".into(),
            name: "unknown".into(),
            arguments: json!({}),
        }]);
        let max_trace = RecordingSink::default();
        let max_runner = AgentRunner::with_trace(
            &max_client,
            &tools,
            "test-model",
            &DefaultPolicy,
            &approver,
            Uuid::from_u128(4),
            &max_trace,
        );
        max_runner
            .run(&mut AgentState::with_max_steps("task", 1))
            .await
            .unwrap();
        assert_eq!(
            max_trace.events().last(),
            Some(&TraceEventKind::MaxStepsReached { step: 1 })
        );

        let fatal_client = FakeClient {
            responses: Mutex::new(VecDeque::from([ModelResponse {
                content: Some("   ".into()),
                tool_calls: vec![],
            }])),
            requests: Mutex::new(Vec::new()),
        };
        let fatal_trace = RecordingSink::default();
        let fatal_runner = AgentRunner::with_trace(
            &fatal_client,
            &tools,
            "test-model",
            &DefaultPolicy,
            &approver,
            Uuid::from_u128(5),
            &fatal_trace,
        );
        let mut state = AgentState::new("task");
        assert!(fatal_runner.run(&mut state).await.is_err());
        assert_eq!(state.status, AgentStatus::Failed);
        assert!(matches!(
            fatal_trace.events().last(),
            Some(TraceEventKind::AgentFailed { step: 1, .. })
        ));
    }

    #[tokio::test]
    async fn checkpoints_transitions_in_order_and_resume_continues_pending_turn() {
        let tools = ToolRegistry::new();
        let client = FakeClient {
            responses: Mutex::new(VecDeque::from([ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
            }])),
            requests: Mutex::new(Vec::new()),
        };
        let approver = FakeApprover::new([]);
        let trace = RecordingSink::default();
        let checkpoints = RecordingSessionSink::default();
        let runner = AgentRunner::with_persistence(
            &client,
            &tools,
            "test-model",
            &DefaultPolicy,
            &approver,
            RunPersistence {
                session_id: Uuid::from_u128(6),
                trace: &trace,
                sessions: &checkpoints,
            },
        );
        runner.run(&mut AgentState::new("task")).await.unwrap();
        let states = checkpoints.states();
        assert_eq!(states[0].status, AgentStatus::Running);
        assert_eq!(states[0].step, 0);
        assert_eq!(states[1].step, 1);
        assert_eq!(states.last().unwrap().status, AgentStatus::Completed);

        let (tools, executions) = counting_registry();
        let resumed_client = FakeClient {
            responses: Mutex::new(VecDeque::from([ModelResponse {
                content: Some("resumed done".into()),
                tool_calls: vec![],
            }])),
            requests: Mutex::new(Vec::new()),
        };
        let resumed_trace = RecordingSink::default();
        let resumed_checkpoints = RecordingSessionSink::default();
        let resumed_policy = FixedPolicy(ToolPermission::Allow);
        let resumed_runner = AgentRunner::with_persistence(
            &resumed_client,
            &tools,
            "test-model",
            &resumed_policy,
            &approver,
            RunPersistence {
                session_id: Uuid::from_u128(7),
                trace: &resumed_trace,
                sessions: &resumed_checkpoints,
            },
        );
        let call = count_call("pending");
        let mut resumed_state = AgentState::new("task");
        resumed_state.step = 1;
        resumed_state
            .messages
            .push(Message::assistant_tool_calls(None, std::slice::from_ref(&call)).unwrap());
        resumed_state.pending_tool_calls.push(call);
        resumed_runner.resume(&mut resumed_state).await.unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(resumed_state.step, 2);
        assert_eq!(resumed_state.status, AgentStatus::Completed);
        assert_eq!(resumed_trace.events()[0], TraceEventKind::SessionResumed);
    }

    #[tokio::test]
    async fn checkpoint_failure_is_fatal_and_traced() {
        let tools = ToolRegistry::new();
        let client = FakeClient {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        };
        let approver = FakeApprover::new([]);
        let trace = RecordingSink::default();
        let runner = AgentRunner::with_persistence(
            &client,
            &tools,
            "test-model",
            &DefaultPolicy,
            &approver,
            RunPersistence {
                session_id: Uuid::from_u128(8),
                trace: &trace,
                sessions: &FailingSessionSink,
            },
        );
        let mut state = AgentState::new("task");
        assert!(runner.run(&mut state).await.is_err());
        assert_eq!(state.status, AgentStatus::Failed);
        assert!(matches!(
            trace.events().last(),
            Some(TraceEventKind::AgentFailed { step: 0, .. })
        ));
    }
}

#[cfg(test)]
mod execution_journal_tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        llm::{ChatRequest, ModelResponse},
        policy::{ApprovalDecision, ToolPermission},
        tools::Tool,
    };

    struct FinalClient;
    #[async_trait]
    impl LlmClient for FinalClient {
        async fn chat(&self, _: ChatRequest) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
            })
        }
    }
    struct ScriptedClient(Mutex<Vec<ModelResponse>>);
    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat(&self, _: ChatRequest) -> Result<ModelResponse> {
            Ok(self.0.lock().unwrap().remove(0))
        }
    }
    struct Allow;
    impl Policy for Allow {
        fn permission(&self, _: &ToolCall) -> ToolPermission {
            ToolPermission::Allow
        }
    }
    struct NoApproval;
    impl ApprovalHandler for NoApproval {
        fn request_approval(&self, _: &ToolCall) -> Result<ApprovalDecision> {
            unreachable!()
        }
    }
    struct NamedTool {
        name: &'static str,
        executions: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "test side effect"
        }
        fn schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, _: Value) -> Result<ToolOutput> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: "side effect".into(),
                success: true,
            })
        }
    }
    #[derive(Default)]
    struct Sessions(Mutex<Vec<AgentState>>);
    impl SessionSink for Sessions {
        fn checkpoint(&self, state: &AgentState) -> Result<()> {
            self.0.lock().unwrap().push(state.clone());
            Ok(())
        }
    }
    #[derive(Default)]
    struct Traces(Mutex<Vec<TraceEventKind>>);
    impl TraceSink for Traces {
        fn record(&self, event: &TraceEvent) -> Result<()> {
            self.0.lock().unwrap().push(event.kind.clone());
            Ok(())
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: format!("{name}-id"),
            name: name.into(),
            arguments: json!({}),
        }
    }

    #[tokio::test]
    async fn resume_never_replays_indeterminate_write_or_shell_calls() {
        for name in ["write_file", "shell"] {
            let executions = Arc::new(AtomicUsize::new(0));
            let mut tools = ToolRegistry::new();
            tools
                .register(NamedTool {
                    name,
                    executions: Arc::clone(&executions),
                })
                .unwrap();
            let sessions = Sessions::default();
            let traces = Traces::default();
            let runner = AgentRunner::with_persistence(
                &FinalClient,
                &tools,
                "model",
                &Allow,
                &NoApproval,
                RunPersistence {
                    session_id: Uuid::nil(),
                    trace: &traces,
                    sessions: &sessions,
                },
            );
            let tool_call = call(name);
            let mut state = AgentState::new("task");
            state.messages.push(
                Message::assistant_tool_calls(None, std::slice::from_ref(&tool_call)).unwrap(),
            );
            state.pending_tool_calls.push(tool_call.clone());
            state.in_flight_tool_call = Some(tool_call.clone());

            runner.resume(&mut state).await.unwrap();

            assert_eq!(executions.load(Ordering::SeqCst), 0, "{name} replayed");
            assert!(state.in_flight_tool_call.is_none());
            assert!(state.pending_tool_calls.is_empty());
            assert!(state.messages.iter().any(|message| {
                message.tool_call_id.as_deref() == Some(tool_call.id.as_str())
                    && message
                        .content
                        .as_deref()
                        .is_some_and(|content| content.contains("indeterminate"))
            }));
            assert!(traces.0.lock().unwrap().iter().any(|event| matches!(event, TraceEventKind::ToolOutcomeIndeterminate { id, .. } if id == &tool_call.id)));
        }
    }

    #[tokio::test]
    async fn dispatch_marker_precedes_side_effect_and_completion_is_atomic() {
        for name in ["write_file", "shell"] {
            let executions = Arc::new(AtomicUsize::new(0));
            let mut tools = ToolRegistry::new();
            tools
                .register(NamedTool {
                    name,
                    executions: Arc::clone(&executions),
                })
                .unwrap();
            let tool_call = call(name);
            let client = ScriptedClient(Mutex::new(vec![
                ModelResponse {
                    content: None,
                    tool_calls: vec![tool_call.clone()],
                },
                ModelResponse {
                    content: Some("done".into()),
                    tool_calls: vec![],
                },
            ]));
            let sessions = Sessions::default();
            let traces = Traces::default();
            let runner = AgentRunner::with_persistence(
                &client,
                &tools,
                "model",
                &Allow,
                &NoApproval,
                RunPersistence {
                    session_id: Uuid::nil(),
                    trace: &traces,
                    sessions: &sessions,
                },
            );
            runner.run(&mut AgentState::new("task")).await.unwrap();
            assert_eq!(executions.load(Ordering::SeqCst), 1);
            let states = sessions.0.lock().unwrap();
            assert!(
                states
                    .iter()
                    .any(|state| state.in_flight_tool_call.as_ref() == Some(&tool_call))
            );
            assert!(states.iter().any(|state| {
                state.in_flight_tool_call.is_none()
                    && state.pending_tool_calls.is_empty()
                    && state.messages.iter().any(|message| {
                        message.tool_call_id.as_deref() == Some(tool_call.id.as_str())
                    })
            }));
        }
    }

    #[test]
    fn abort_checkpoints_terminal_status_and_trace() {
        let sessions = Sessions::default();
        let traces = Traces::default();
        let tools = ToolRegistry::new();
        let runner = AgentRunner::with_persistence(
            &FinalClient,
            &tools,
            "model",
            &Allow,
            &NoApproval,
            RunPersistence {
                session_id: Uuid::nil(),
                trace: &traces,
                sessions: &sessions,
            },
        );
        let mut state = AgentState::new("task");
        runner.abort(&mut state).unwrap();
        assert_eq!(state.status, AgentStatus::Aborted);
        assert_eq!(
            sessions.0.lock().unwrap().last().unwrap().status,
            AgentStatus::Aborted
        );
        assert!(matches!(
            traces.0.lock().unwrap().last(),
            Some(TraceEventKind::AgentAborted { step: 0 })
        ));
    }
}
