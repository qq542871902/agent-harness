use anyhow::{Context, Result};
use tracing::info;

use crate::{
    llm::{ChatRequest, LlmClient, Message},
    tools::{ToolOutput, ToolRegistry},
};

use super::{AgentState, AgentStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    Completed { content: String },
    MaxStepsReached { steps: usize },
}

/// Drives bounded model → tool → observation turns for one agent task.
pub struct AgentRunner<'a> {
    client: &'a dyn LlmClient,
    tools: &'a ToolRegistry,
    model: &'a str,
}

impl<'a> AgentRunner<'a> {
    pub fn new(client: &'a dyn LlmClient, tools: &'a ToolRegistry, model: &'a str) -> Self {
        Self {
            client,
            tools,
            model,
        }
    }

    pub async fn run(&self, state: &mut AgentState) -> Result<AgentOutcome> {
        state.status = AgentStatus::Running;

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
                let observation = match self.tools.execute(&call).await {
                    Ok(ToolOutput {
                        content,
                        success: true,
                    }) => content,
                    Ok(ToolOutput {
                        content,
                        success: false,
                    }) => format!("Tool execution failed:\n{content}"),
                    Err(error) => format!("Tool execution failed:\n{error:#}"),
                };
                state
                    .messages
                    .push(Message::tool_result(call.id, observation));
            }
        }

        state.status = AgentStatus::MaxStepsReached;
        Ok(AgentOutcome::MaxStepsReached { steps: state.step })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, fs, sync::Mutex};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::{
        llm::{ModelResponse, Role, ToolCall},
        test_support::TestWorkspace,
        tools::ReadFileTool,
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

    #[tokio::test]
    async fn multi_turn_flow_replays_tool_observation() {
        let workspace = TestWorkspace::new("runner");
        fs::write(workspace.path().join("note.txt"), "inspected").unwrap();
        let mut tools = ToolRegistry::new();
        tools
            .register(ReadFileTool::new(workspace.path()).unwrap())
            .unwrap();
        let client = FakeClient {
            responses: Mutex::new(VecDeque::from([
                ModelResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "note.txt"}),
                    }],
                },
                ModelResponse {
                    content: Some("done".into()),
                    tool_calls: vec![],
                },
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let runner = AgentRunner::new(&client, &tools, "test-model");
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
            requests[1].messages[3].content.as_deref(),
            Some("inspected")
        );
    }

    #[tokio::test]
    async fn tool_failures_are_observations() {
        let tools = ToolRegistry::new();
        let client = FakeClient {
            responses: Mutex::new(VecDeque::from([
                ModelResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "missing".into(),
                        name: "unknown".into(),
                        arguments: json!({}),
                    }],
                },
                ModelResponse {
                    content: Some("recovered".into()),
                    tool_calls: vec![],
                },
            ])),
            requests: Mutex::new(Vec::new()),
        };
        let runner = AgentRunner::new(&client, &tools, "test-model");
        let mut state = AgentState::new("task");

        runner.run(&mut state).await.unwrap();

        let requests = client.requests.lock().unwrap();
        assert!(
            requests[1].messages[3]
                .content
                .as_deref()
                .unwrap()
                .contains("unknown tool")
        );
    }
}
