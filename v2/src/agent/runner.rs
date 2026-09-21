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
                let content = response
                    .content
                    .filter(|content| !content.trim().is_empty())
                    .context("model returned a final response without content")?;
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
