use anyhow::Result;
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    context::{ContextBuilder, ContextSettings, truncate_tool_output},
    llm::LlmClient,
    policy::{ApprovalHandler, Policy},
    session::NULL_SESSION_SINK,
    tools::{
        MAX_SUB_AGENT_OUTPUT_CHARS, SpawnAgentRequest, SubAgentRole, SubAgentSpawner, ToolOutput,
        ToolRegistry,
    },
    trace::{TraceActor, TraceEvent, TraceEventKind, TraceSink},
};

use super::{AgentOutcome, AgentRunner, AgentState, RunPersistence};

const SUB_AGENT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct SameProcessSubAgentSpawner {
    client: Arc<dyn LlmClient>,
    base_tools: ToolRegistry,
    model: String,
    policy: Arc<dyn Policy>,
    approver: Arc<dyn ApprovalHandler>,
    context_settings: ContextSettings,
    sensitive_values: Vec<String>,
    session_id: Uuid,
    trace: Arc<dyn TraceSink>,
    timeout: Duration,
}

impl SameProcessSubAgentSpawner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<dyn LlmClient>,
        base_tools: ToolRegistry,
        model: impl Into<String>,
        policy: Arc<dyn Policy>,
        approver: Arc<dyn ApprovalHandler>,
        context_settings: ContextSettings,
        sensitive_values: Vec<String>,
        session_id: Uuid,
        trace: Arc<dyn TraceSink>,
    ) -> Self {
        Self {
            client,
            base_tools,
            model: model.into(),
            policy,
            approver,
            context_settings,
            sensitive_values,
            session_id,
            trace,
            timeout: SUB_AGENT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn tools_for(&self, role: SubAgentRole) -> ToolRegistry {
        self.base_tools.filtered(|name| {
            crate::mcp::parse_namespaced_name(name).is_some()
                || match role {
                    SubAgentRole::Research => matches!(name, "read_file" | "list_files"),
                    SubAgentRole::Code => {
                        matches!(name, "read_file" | "list_files" | "write_file" | "shell")
                    }
                    SubAgentRole::Test => matches!(name, "read_file" | "list_files" | "shell"),
                }
        })
    }

    fn system_prompt(role: SubAgentRole, task: &str, capabilities: &[String]) -> String {
        format!(
            "You are a bounded {role} sub-agent delegated this task:\n{task}\n\nAvailable capabilities: {capabilities}. Use only these tools. Tool calls remain policy checked and may require approval. Complete only the delegated task and return a concise final answer.",
            role = role.as_str(),
            capabilities = capabilities.join(", ")
        )
    }

    fn emit(&self, kind: TraceEventKind) -> Result<()> {
        self.trace.record(&TraceEvent::new(self.session_id, kind))
    }
}

struct ChildTrace {
    inner: Arc<dyn TraceSink>,
    child_id: Uuid,
    parent_tool_call_id: String,
}

impl TraceSink for ChildTrace {
    fn record(&self, event: &TraceEvent) -> Result<()> {
        let mut child_event = event.clone();
        child_event.actor = TraceActor::Child {
            child_id: self.child_id,
            parent_tool_call_id: self.parent_tool_call_id.clone(),
        };
        self.inner.record(&child_event)
    }
}

#[async_trait]
impl SubAgentSpawner for SameProcessSubAgentSpawner {
    async fn spawn(&self, parent_tool_call_id: &str, request: SpawnAgentRequest) -> ToolOutput {
        let child_id = Uuid::new_v4();
        let role = request.role.as_str().to_owned();
        if let Err(error) = self.emit(TraceEventKind::SubAgentStarted {
            child_id,
            parent_tool_call_id: parent_tool_call_id.to_owned(),
            role: role.clone(),
            steps: 0,
            status: "started".into(),
        }) {
            return ToolOutput {
                content: format!("Sub-agent failed before start: {error:#}"),
                success: false,
            };
        }

        let tools = self.tools_for(request.role);
        let capability_names = tools.names();
        debug_assert!(!tools.contains("spawn_agent"));
        let system_prompt = Self::system_prompt(request.role, &request.task, &capability_names);
        let mut state = AgentState::with_system_prompt(
            system_prompt,
            "Complete the delegated task.",
            request.max_steps,
        );
        let child_trace = ChildTrace {
            inner: Arc::clone(&self.trace),
            child_id,
            parent_tool_call_id: parent_tool_call_id.to_owned(),
        };
        let context = match ContextBuilder::with_sensitive_values(
            self.context_settings.clone(),
            self.sensitive_values.clone(),
        ) {
            Ok(context) => context,
            Err(error) => {
                let _ = self.emit(TraceEventKind::SubAgentFailed {
                    child_id,
                    parent_tool_call_id: parent_tool_call_id.to_owned(),
                    role,
                    steps: 0,
                    status: "setup_failed".into(),
                });
                return bounded_failure(format!("Sub-agent setup failed: {error:#}"));
            }
        };
        let runner = AgentRunner::with_persistence(
            self.client.as_ref(),
            &tools,
            &self.model,
            self.policy.as_ref(),
            self.approver.as_ref(),
            context,
            RunPersistence {
                session_id: self.session_id,
                trace: &child_trace,
                sessions: &NULL_SESSION_SINK,
            },
        );

        match timeout(self.timeout, runner.run(&mut state)).await {
            Ok(Ok(AgentOutcome::Completed { content })) => {
                let _ = self.emit(TraceEventKind::SubAgentCompleted {
                    child_id,
                    parent_tool_call_id: parent_tool_call_id.to_owned(),
                    role,
                    steps: state.step,
                    status: "completed".into(),
                });
                bounded(content, true)
            }
            Ok(Ok(AgentOutcome::MaxStepsReached { steps })) => {
                let _ = self.emit(TraceEventKind::SubAgentCompleted {
                    child_id,
                    parent_tool_call_id: parent_tool_call_id.to_owned(),
                    role,
                    steps,
                    status: "max_steps_reached".into(),
                });
                bounded(
                    format!(
                        "Sub-agent reached its maximum of {steps} steps without a final answer."
                    ),
                    false,
                )
            }
            Ok(Err(error)) => {
                let _ = self.emit(TraceEventKind::SubAgentFailed {
                    child_id,
                    parent_tool_call_id: parent_tool_call_id.to_owned(),
                    role,
                    steps: state.step,
                    status: "failed".into(),
                });
                bounded_failure(format!(
                    "Sub-agent failed after {} steps: {error:#}",
                    state.step
                ))
            }
            Err(_) => {
                let _ = self.emit(TraceEventKind::SubAgentFailed {
                    child_id,
                    parent_tool_call_id: parent_tool_call_id.to_owned(),
                    role,
                    steps: state.step,
                    status: "timed_out".into(),
                });
                bounded_failure("Sub-agent timed out.".into())
            }
        }
    }
}

fn bounded_failure(content: String) -> ToolOutput {
    bounded(content, false)
}

fn bounded(content: String, success: bool) -> ToolOutput {
    ToolOutput {
        content: truncate_tool_output(&content, MAX_SUB_AGENT_OUTPUT_CHARS).content,
        success,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::ContextSettings,
        llm::{ChatRequest, ModelResponse, ToolCall},
        policy::{ApprovalDecision, DefaultPolicy},
        session::NULL_SESSION_SINK,
        tools::{SpawnAgentTool, Tool, ToolRegistry},
        trace::TraceEventKind,
    };
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::{collections::VecDeque, sync::Mutex};

    struct ScriptedClient {
        responses: Mutex<VecDeque<Result<ModelResponse>>>,
        requests: Mutex<Vec<ChatRequest>>,
    }
    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat(&self, request: ChatRequest) -> Result<ModelResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("script exhausted")))
        }
    }

    #[derive(Default)]
    struct RecordingTrace(Mutex<Vec<TraceEventKind>>);
    impl TraceSink for RecordingTrace {
        fn record(&self, event: &TraceEvent) -> Result<()> {
            self.0.lock().unwrap().push(event.kind.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct EventRecordingTrace(Mutex<Vec<TraceEvent>>);
    impl TraceSink for EventRecordingTrace {
        fn record(&self, event: &TraceEvent) -> Result<()> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[test]
    fn child_trace_scopes_every_internal_event() {
        let captured = Arc::new(EventRecordingTrace::default());
        let trace = ChildTrace {
            inner: captured.clone(),
            child_id: Uuid::from_u128(42),
            parent_tool_call_id: "parent-call".into(),
        };
        trace
            .record(&TraceEvent::new(
                Uuid::from_u128(1),
                TraceEventKind::SessionStarted,
            ))
            .unwrap();
        let events = captured.0.lock().unwrap();
        assert!(matches!(
            events[0].actor,
            TraceActor::Child {
                child_id,
                ref parent_tool_call_id
            } if child_id == Uuid::from_u128(42) && parent_tool_call_id == "parent-call"
        ));
        assert!(matches!(events[0].kind, TraceEventKind::SessionStarted));
    }

    struct DecisionApprover(ApprovalDecision);
    #[async_trait]
    impl ApprovalHandler for DecisionApprover {
        async fn request_approval(&self, _: &ToolCall) -> Result<ApprovalDecision> {
            Ok(self.0)
        }
    }

    struct PendingApprover;
    #[async_trait]
    impl ApprovalHandler for PendingApprover {
        async fn request_approval(&self, _: &ToolCall) -> Result<ApprovalDecision> {
            std::future::pending().await
        }
    }

    struct MarkerTool(&'static str);
    #[async_trait]
    impl Tool for MarkerTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "marker"
        }
        fn schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, _: Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "ok".into(),
                success: true,
            })
        }
    }

    fn response(content: &str) -> Result<ModelResponse> {
        Ok(ModelResponse {
            content: Some(content.into()),
            tool_calls: vec![],
        })
    }

    fn spawn_response(role: &str, task: &str, max_steps: usize) -> Result<ModelResponse> {
        Ok(ModelResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "parent-spawn-1".into(),
                name: "spawn_agent".into(),
                arguments: json!({"role":role,"task":task,"max_steps":max_steps}),
            }],
        })
    }

    fn base_registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        for name in [
            "read_file",
            "list_files",
            "write_file",
            "shell",
            "mcp__local__echo",
        ] {
            tools.register(MarkerTool(name)).unwrap();
        }
        tools
    }

    fn spawner(
        client: Arc<dyn LlmClient>,
        trace: Arc<dyn TraceSink>,
        approver: Arc<dyn ApprovalHandler>,
    ) -> SameProcessSubAgentSpawner {
        SameProcessSubAgentSpawner::new(
            client,
            base_registry(),
            "model",
            Arc::new(DefaultPolicy),
            approver,
            ContextSettings::default(),
            vec![],
            Uuid::from_u128(99),
            trace,
        )
    }

    #[tokio::test]
    async fn parent_spawn_child_final_parent_final_and_payload_free_ordering() {
        let client = Arc::new(ScriptedClient {
            responses: Mutex::new(VecDeque::from([
                spawn_response("research", "secret delegated task", 2),
                response("child secret output"),
                response("parent final"),
            ])),
            requests: Mutex::new(vec![]),
        });
        let trace = Arc::new(RecordingTrace::default());
        let spawner: Arc<dyn SubAgentSpawner> = Arc::new(spawner(
            client.clone(),
            trace.clone(),
            Arc::new(DecisionApprover(ApprovalDecision::GrantOnce)),
        ));
        let mut root_tools = base_registry();
        root_tools.register(SpawnAgentTool::new(spawner)).unwrap();
        let runner = AgentRunner::with_persistence(
            client.as_ref(),
            &root_tools,
            "model",
            &DefaultPolicy,
            &DecisionApprover(ApprovalDecision::GrantOnce),
            ContextBuilder::new(ContextSettings::default()).unwrap(),
            RunPersistence {
                session_id: Uuid::from_u128(99),
                trace: trace.as_ref(),
                sessions: &NULL_SESSION_SINK,
            },
        );
        let mut state = AgentState::new("parent task");
        assert_eq!(
            runner.run(&mut state).await.unwrap(),
            AgentOutcome::Completed {
                content: "parent final".into()
            }
        );
        assert!(state.messages.iter().any(|message| {
            message.content.as_deref() == Some("child secret output")
                && message.tool_call_id.as_deref() == Some("parent-spawn-1")
        }));

        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let child_names = requests[1]
            .tools
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(child_names, ["list_files", "mcp__local__echo", "read_file"]);
        assert!(!child_names.contains(&"spawn_agent"));
        assert!(
            requests[1].messages[0]
                .content
                .as_deref()
                .unwrap()
                .contains("secret delegated task")
        );
        drop(requests);

        let events = trace.0.lock().unwrap();
        let started = events
            .iter()
            .position(|event| matches!(event, TraceEventKind::SubAgentStarted { .. }))
            .unwrap();
        let completed = events
            .iter()
            .position(|event| matches!(event, TraceEventKind::SubAgentCompleted { .. }))
            .unwrap();
        let result = events
            .iter()
            .rposition(|event| matches!(event, TraceEventKind::ToolResult { name, .. } if name == "spawn_agent"))
            .unwrap();
        assert!(started < completed && completed < result);
        let encoded = serde_json::to_string(&*events).unwrap();
        assert!(!encoded.contains("secret delegated task"));
        assert!(!encoded.contains("child secret output"));
    }

    #[test]
    fn role_capabilities_are_exact_and_recursion_is_absent() {
        let client: Arc<dyn LlmClient> = Arc::new(ScriptedClient {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(vec![]),
        });
        let runtime = spawner(
            client,
            Arc::new(RecordingTrace::default()),
            Arc::new(DecisionApprover(ApprovalDecision::GrantOnce)),
        );
        assert_eq!(
            runtime.tools_for(SubAgentRole::Research).names(),
            ["list_files", "mcp__local__echo", "read_file"]
        );
        assert_eq!(
            runtime.tools_for(SubAgentRole::Code).names(),
            [
                "list_files",
                "mcp__local__echo",
                "read_file",
                "shell",
                "write_file"
            ]
        );
        assert_eq!(
            runtime.tools_for(SubAgentRole::Test).names(),
            ["list_files", "mcp__local__echo", "read_file", "shell"]
        );
        for role in [
            SubAgentRole::Research,
            SubAgentRole::Code,
            SubAgentRole::Test,
        ] {
            assert!(!runtime.tools_for(role).contains("spawn_agent"));
        }
    }

    #[tokio::test]
    async fn max_steps_and_fatal_errors_become_bounded_observations() {
        let max_client: Arc<dyn LlmClient> = Arc::new(ScriptedClient {
            responses: Mutex::new(VecDeque::from([Ok(ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "read".into(),
                    name: "read_file".into(),
                    arguments: json!({}),
                }],
            })])),
            requests: Mutex::new(vec![]),
        });
        let output = spawner(
            max_client,
            Arc::new(RecordingTrace::default()),
            Arc::new(DecisionApprover(ApprovalDecision::GrantOnce)),
        )
        .spawn(
            "max",
            SpawnAgentRequest {
                role: SubAgentRole::Research,
                task: "stop".into(),
                max_steps: 1,
            },
        )
        .await;
        assert!(!output.success && output.content.contains("maximum of 1 steps"));

        let fatal_client: Arc<dyn LlmClient> = Arc::new(ScriptedClient {
            responses: Mutex::new(VecDeque::from([Err(anyhow!("fatal model error"))])),
            requests: Mutex::new(vec![]),
        });
        let output = spawner(
            fatal_client,
            Arc::new(RecordingTrace::default()),
            Arc::new(DecisionApprover(ApprovalDecision::GrantOnce)),
        )
        .spawn(
            "fatal",
            SpawnAgentRequest {
                role: SubAgentRole::Test,
                task: "fail".into(),
                max_steps: 1,
            },
        )
        .await;
        assert!(!output.success && output.content.contains("fatal model error"));
        assert!(output.content.chars().count() <= MAX_SUB_AGENT_OUTPUT_CHARS);
    }

    #[tokio::test]
    async fn nonresponsive_child_approval_is_cancelled_by_runtime_deadline() {
        let client: Arc<dyn LlmClient> = Arc::new(ScriptedClient {
            responses: Mutex::new(VecDeque::from([Ok(ModelResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "write".into(),
                    name: "write_file".into(),
                    arguments: json!({}),
                }],
            })])),
            requests: Mutex::new(vec![]),
        });
        let output = spawner(
            client,
            Arc::new(RecordingTrace::default()),
            Arc::new(PendingApprover),
        )
        .with_timeout(Duration::from_millis(20))
        .spawn(
            "approval-timeout",
            SpawnAgentRequest {
                role: SubAgentRole::Code,
                task: "request approval".into(),
                max_steps: 2,
            },
        )
        .await;
        assert!(!output.success);
        assert_eq!(output.content, "Sub-agent timed out.");
    }

    #[tokio::test]
    async fn parent_approval_denial_prevents_child_execution() {
        let client = Arc::new(ScriptedClient {
            responses: Mutex::new(VecDeque::from([
                spawn_response("code", "do not run", 2),
                response("continued safely"),
            ])),
            requests: Mutex::new(vec![]),
        });
        let trace = Arc::new(RecordingTrace::default());
        let child_runtime: Arc<dyn SubAgentSpawner> = Arc::new(spawner(
            client.clone(),
            trace.clone(),
            Arc::new(DecisionApprover(ApprovalDecision::GrantOnce)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(SpawnAgentTool::new(child_runtime)).unwrap();
        let runner = AgentRunner::with_persistence(
            client.as_ref(),
            &tools,
            "model",
            &DefaultPolicy,
            &DecisionApprover(ApprovalDecision::Deny),
            ContextBuilder::new(ContextSettings::default()).unwrap(),
            RunPersistence {
                session_id: Uuid::from_u128(99),
                trace: trace.as_ref(),
                sessions: &NULL_SESSION_SINK,
            },
        );
        let mut state = AgentState::new("parent");
        runner.run(&mut state).await.unwrap();
        assert_eq!(client.requests.lock().unwrap().len(), 2);
        assert!(
            !trace
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, TraceEventKind::SubAgentStarted { .. }))
        );
    }
}
