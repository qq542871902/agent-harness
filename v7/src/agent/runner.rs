use super::{AgentState, AgentStatus};
#[cfg(test)]
use crate::session::NULL_SESSION_SINK;
use crate::{
    context::{ContextBuilder, ContextMetadata, truncate_tool_output},
    llm::{LlmClient, Message, ToolCall},
    policy::{ApprovalDecision, ApprovalHandler, ApprovalState, Policy, ToolPermission},
    session::SessionSink,
    tools::{ToolOutput, ToolRegistry},
    trace::{TraceEvent, TraceEventKind, TraceSink, safe_error},
};
use anyhow::{Context, Result};
use tracing::info;
use uuid::Uuid;
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
pub struct AgentRunner<'a> {
    client: &'a dyn LlmClient,
    tools: &'a ToolRegistry,
    model: &'a str,
    policy: &'a dyn Policy,
    approver: &'a dyn ApprovalHandler,
    context: ContextBuilder,
    session_id: Uuid,
    trace: &'a dyn TraceSink,
    sessions: &'a dyn SessionSink,
}
struct Observation {
    content: String,
    success: bool,
    original_chars: usize,
    retained_chars: usize,
    truncated: bool,
}
impl Observation {
    fn unbounded(content: String, success: bool) -> Self {
        let chars = content.chars().count();
        Self {
            content,
            success,
            original_chars: chars,
            retained_chars: chars,
            truncated: false,
        }
    }
}
impl<'a> AgentRunner<'a> {
    pub fn with_persistence(
        client: &'a dyn LlmClient,
        tools: &'a ToolRegistry,
        model: &'a str,
        policy: &'a dyn Policy,
        approver: &'a dyn ApprovalHandler,
        context: ContextBuilder,
        persistence: RunPersistence<'a>,
    ) -> Self {
        Self {
            client,
            tools,
            model,
            policy,
            approver,
            context,
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
        if let Err(error) = self.emit(if resumed {
            TraceEventKind::SessionResumed
        } else {
            TraceEventKind::SessionStarted
        }) {
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
            let built = match self.context.build(
                self.model,
                &state.messages,
                self.tools.definitions(),
                state.step,
            ) {
                Ok(built) => built,
                Err(error) => return self.fail(state, error),
            };
            state.context_summary = built.summary.clone();
            if let Err(error) = self.checkpoint(state) {
                return self.fail_checkpoint(state, error);
            }
            if let Err(error) = self.emit_context(state.step, &built.metadata) {
                return self.fail(state, error);
            }
            if let Err(error) = self.emit(TraceEventKind::ModelRequest { step: state.step }) {
                return self.fail(state, error);
            }
            let response = match self.client.chat(built.request).await {
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
                    .filter(|c| !c.trim().is_empty())
                    .context("model returned a final response without content")
                {
                    Ok(c) => c,
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
            let assistant = match Message::assistant_tool_calls(response.content, &calls) {
                Ok(m) => m,
                Err(error) => return self.fail(state, error),
            };
            state.messages.push(assistant);
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
    fn emit_context(&self, step: usize, m: &ContextMetadata) -> Result<()> {
        self.emit(TraceEventKind::ContextBuilt {
            step,
            token_budget: m.token_budget,
            estimated_tokens: m.estimated_tokens,
            original_messages: m.original_messages,
            retained_messages: m.retained_messages,
            omitted_messages: m.omitted_messages,
            omitted_groups: m.omitted_groups,
            compacted: m.compacted,
        })
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
            let observation = self
                .policy_checked_observation(state, approvals, &call)
                .await?;
            state
                .messages
                .push(Message::tool_result(call.id.clone(), observation.content));
            state.pending_tool_calls.remove(0);
            state.in_flight_tool_call = None;
            self.checkpoint(state)?;
            self.emit(TraceEventKind::ToolResult {
                id: call.id,
                name: call.name,
                success: observation.success,
                original_chars: observation.original_chars,
                retained_chars: observation.retained_chars,
                truncated: observation.truncated,
            })?;
        }
        Ok(())
    }
    async fn policy_checked_observation(
        &self,
        state: &mut AgentState,
        approvals: &mut ApprovalState,
        call: &ToolCall,
    ) -> Result<Observation> {
        let observation = match self.policy.permission(call) {
            ToolPermission::Deny => Observation::unbounded(
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
                        Observation::unbounded(
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
                        Observation::unbounded(
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
        Ok(observation)
    }
    async fn execute_observation(
        &self,
        state: &mut AgentState,
        call: &ToolCall,
    ) -> Result<Observation> {
        state.in_flight_tool_call = Some(call.clone());
        self.checkpoint(state)?;
        let (content, success) = match self.tools.execute(call).await {
            Ok(ToolOutput {
                content,
                success: true,
            }) => (content, true),
            Ok(ToolOutput {
                content,
                success: false,
            }) => (format!("Tool execution failed:\n{content}"), false),
            Err(error) => (format!("Tool execution failed:\n{error:#}"), false),
        };
        let truncated =
            truncate_tool_output(&content, self.context.settings().max_tool_output_chars);
        Ok(Observation {
            content: truncated.content,
            success,
            original_chars: truncated.original_chars,
            retained_chars: truncated.retained_chars,
            truncated: truncated.truncated,
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
    use super::*;
    use crate::{
        context::ContextSettings,
        llm::{ChatRequest, ModelResponse, Role},
        policy::DefaultPolicy,
        tools::Tool,
    };
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::{collections::VecDeque, sync::Mutex};
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
                .ok_or_else(|| anyhow!("no response"))
        }
    }
    struct Approver;
    impl ApprovalHandler for Approver {
        fn request_approval(&self, _: &ToolCall) -> Result<ApprovalDecision> {
            Ok(ApprovalDecision::GrantOnce)
        }
    }
    struct LargeTool;
    #[async_trait]
    impl Tool for LargeTool {
        fn name(&self) -> &'static str {
            "read_file"
        }
        fn description(&self) -> &'static str {
            "large"
        }
        fn schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, _: Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "😀".repeat(500),
                success: true,
            })
        }
    }
    #[derive(Default)]
    struct Recording(Mutex<Vec<TraceEventKind>>);
    impl TraceSink for Recording {
        fn record(&self, event: &TraceEvent) -> Result<()> {
            self.0.lock().unwrap().push(event.kind.clone());
            Ok(())
        }
    }
    #[tokio::test]
    async fn runner_uses_context_builder_and_truncates_native_observation() {
        let mut tools = ToolRegistry::new();
        tools.register(LargeTool).unwrap();
        let client = FakeClient {
            responses: Mutex::new(VecDeque::from([
                ModelResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "x".into(),
                        name: "read_file".into(),
                        arguments: json!({}),
                    }],
                },
                ModelResponse {
                    content: Some("done".into()),
                    tool_calls: vec![],
                },
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let trace = Recording::default();
        let settings = ContextSettings {
            token_budget: 16_384,
            max_tool_output_chars: 128,
            max_summary_chars: 512,
        };
        let runner = AgentRunner::with_persistence(
            &client,
            &tools,
            "model",
            &DefaultPolicy,
            &Approver,
            ContextBuilder::new(settings).unwrap(),
            RunPersistence {
                session_id: Uuid::nil(),
                trace: &trace,
                sessions: &NULL_SESSION_SINK,
            },
        );
        let mut state = AgentState::new("task");
        runner.run(&mut state).await.unwrap();
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests[1].messages[3].role, Role::Tool);
        assert_eq!(
            requests[1].messages[3]
                .content
                .as_deref()
                .unwrap()
                .chars()
                .count(),
            128
        );
        assert!(trace.0.lock().unwrap().iter().any(
            |e| matches!(e,TraceEventKind::ContextBuilt{estimated_tokens,..}if *estimated_tokens>0)
        ));
        assert!(trace.0.lock().unwrap().iter().any(|e| matches!(
            e,
            TraceEventKind::ToolResult {
                truncated: true,
                original_chars: 500,
                retained_chars: 128,
                ..
            }
        )));
    }
}

#[cfg(test)]
mod resume_context_tests {
    use super::*;
    use crate::{
        context::ContextSettings,
        llm::{ChatRequest, ModelResponse},
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FinalClient(Mutex<Vec<ChatRequest>>);
    #[async_trait]
    impl LlmClient for FinalClient {
        async fn chat(&self, request: ChatRequest) -> Result<ModelResponse> {
            self.0.lock().unwrap().push(request);
            Ok(ModelResponse {
                content: Some("done".into()),
                tool_calls: vec![],
            })
        }
    }
    struct NoApproval;
    impl ApprovalHandler for NoApproval {
        fn request_approval(&self, _: &ToolCall) -> Result<ApprovalDecision> {
            unreachable!()
        }
    }
    #[derive(Default)]
    struct Trace(Mutex<Vec<TraceEventKind>>);
    impl TraceSink for Trace {
        fn record(&self, event: &TraceEvent) -> Result<()> {
            self.0.lock().unwrap().push(event.kind.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn resume_rebuilds_and_persists_summary_without_rewriting_canonical_history() {
        let client = FinalClient(Mutex::new(Vec::new()));
        let tools = ToolRegistry::new();
        let trace = Trace::default();
        let settings = ContextSettings {
            token_budget: 300,
            max_tool_output_chars: 128,
            max_summary_chars: 512,
        };
        let runner = AgentRunner::with_persistence(
            &client,
            &tools,
            "model",
            &crate::policy::DefaultPolicy,
            &NoApproval,
            ContextBuilder::new(settings).unwrap(),
            RunPersistence {
                session_id: Uuid::from_u128(61),
                trace: &trace,
                sessions: &NULL_SESSION_SINK,
            },
        );
        let mut state = AgentState::new("task");
        state
            .messages
            .push(Message::assistant("old history ".repeat(300)));
        state
            .messages
            .push(Message::assistant("latest coherent turn"));
        let canonical_before = state.messages.clone();
        runner.resume(&mut state).await.unwrap();
        assert_eq!(
            &state.messages[..canonical_before.len()],
            canonical_before.as_slice()
        );
        assert!(
            state
                .context_summary
                .as_ref()
                .is_some_and(|summary| summary.omitted_messages > 0)
        );
        assert!(matches!(
            trace.0.lock().unwrap().first(),
            Some(TraceEventKind::SessionResumed)
        ));
        let requests = client.0.lock().unwrap();
        let request = &requests[0];
        assert_ne!(request.messages, canonical_before);
        assert!(request.messages.iter().any(|message| {
            message.role == crate::llm::Role::System
                && message
                    .content
                    .as_deref()
                    .is_some_and(|content| content.starts_with("Context summary"))
        }));
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
        context::ContextSettings,
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
    fn context() -> ContextBuilder {
        ContextBuilder::new(ContextSettings::default()).unwrap()
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
                context(),
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
                context(),
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
            context(),
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
