use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use super::{Tool, ToolOutput};
use crate::context::truncate_tool_output;

pub const MAX_SUB_AGENT_TASK_CHARS: usize = 4_096;
pub const MAX_SUB_AGENT_OUTPUT_CHARS: usize = 16_384;
pub const MAX_SUB_AGENT_STEPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentRole {
    Research,
    Code,
    Test,
}

impl SubAgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Code => "code",
            Self::Test => "test",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnAgentRequest {
    pub role: SubAgentRole,
    pub task: String,
    pub max_steps: usize,
}

#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    async fn spawn(&self, parent_tool_call_id: &str, request: SpawnAgentRequest) -> ToolOutput;
}

pub struct SpawnAgentTool {
    spawner: Arc<dyn SubAgentSpawner>,
}

impl SpawnAgentTool {
    pub fn new(spawner: Arc<dyn SubAgentSpawner>) -> Self {
        Self { spawner }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArguments {
    role: SubAgentRole,
    task: String,
    #[serde(default = "default_max_steps")]
    max_steps: usize,
}

fn default_max_steps() -> usize {
    4
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Delegate one bounded task to a same-process research, code, or test sub-agent"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "role": {"type": "string", "enum": ["research", "code", "test"]},
                "task": {"type": "string", "minLength": 1, "maxLength": MAX_SUB_AGENT_TASK_CHARS},
                "max_steps": {"type": "integer", "minimum": 1, "maximum": MAX_SUB_AGENT_STEPS}
            },
            "required": ["role", "task"]
        })
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
        bail!("spawn_agent requires tool invocation context")
    }

    async fn execute_for_call(
        &self,
        parent_tool_call_id: &str,
        arguments: Value,
    ) -> Result<ToolOutput> {
        let arguments: SpawnArguments = serde_json::from_value(arguments)
            .context("spawn_agent arguments must match the strict schema")?;
        let task = arguments.task.trim();
        if task.is_empty() {
            bail!("spawn_agent task must not be empty");
        }
        if task.chars().count() > MAX_SUB_AGENT_TASK_CHARS {
            bail!("spawn_agent task exceeds {MAX_SUB_AGENT_TASK_CHARS} characters");
        }
        if !(1..=MAX_SUB_AGENT_STEPS).contains(&arguments.max_steps) {
            bail!("spawn_agent max_steps must be between 1 and {MAX_SUB_AGENT_STEPS}");
        }
        let mut output = self
            .spawner
            .spawn(
                parent_tool_call_id,
                SpawnAgentRequest {
                    role: arguments.role,
                    task: task.to_owned(),
                    max_steps: arguments.max_steps,
                },
            )
            .await;
        output.content = truncate_tool_output(&output.content, MAX_SUB_AGENT_OUTPUT_CHARS).content;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingSpawner(Mutex<Vec<(String, SpawnAgentRequest)>>);
    #[async_trait]
    impl SubAgentSpawner for RecordingSpawner {
        async fn spawn(&self, id: &str, request: SpawnAgentRequest) -> ToolOutput {
            self.0.lock().unwrap().push((id.into(), request));
            ToolOutput {
                content: "x".repeat(MAX_SUB_AGENT_OUTPUT_CHARS + 100),
                success: true,
            }
        }
    }

    #[tokio::test]
    async fn validates_strict_bounded_arguments_and_bounds_output() {
        let spawner = Arc::new(RecordingSpawner(Mutex::new(vec![])));
        let tool = SpawnAgentTool::new(spawner.clone());
        let output = tool
            .execute_for_call(
                "call-7",
                json!({"role":"code","task":" implement ","max_steps":8}),
            )
            .await
            .unwrap();
        assert_eq!(output.content.chars().count(), MAX_SUB_AGENT_OUTPUT_CHARS);
        {
            let calls = spawner.0.lock().unwrap();
            assert_eq!(calls[0].0, "call-7");
            assert_eq!(calls[0].1.task, "implement");
        }

        for arguments in [
            json!({"role":"other","task":"x"}),
            json!({"role":"test","task":" "}),
            json!({"role":"research","task":"x","max_steps":0}),
            json!({"role":"research","task":"x","max_steps":9}),
            json!({"role":"research","task":"x","unknown":true}),
            json!({"role":"research","task":"x".repeat(MAX_SUB_AGENT_TASK_CHARS + 1)}),
        ] {
            assert!(tool.execute_for_call("bad", arguments).await.is_err());
        }
        assert_eq!(spawner.0.lock().unwrap().len(), 1);
    }
}
