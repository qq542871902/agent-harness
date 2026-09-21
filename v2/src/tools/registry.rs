use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};

use crate::llm::{FunctionDefinition, ToolCall, ToolDefinition, ToolDefinitionKind};

use super::{Tool, ToolOutput};

/// Resolves model-requested tool calls to native tool implementations.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self
            .tools
            .values()
            .map(|tool| ToolDefinition {
                kind: ToolDefinitionKind::Function,
                function: FunctionDefinition {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.schema(),
                },
            })
            .collect();
        definitions.sort_by(|left, right| left.function.name.cmp(&right.function.name));
        definitions
    }

    pub async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(&call.name)
            .with_context(|| format!("unknown tool `{}`", call.name))?;

        tool.execute(call.arguments.clone()).await
    }
}
