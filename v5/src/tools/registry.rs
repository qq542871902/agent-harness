use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, bail};

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

    pub fn register<T>(&mut self, tool: T) -> Result<()>
    where
        T: Tool + 'static,
    {
        let name = tool.name();
        if self.tools.contains_key(name) {
            bail!("tool `{name}` is already registered");
        }
        self.tools.insert(name.to_owned(), Arc::new(tool));
        Ok(())
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

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> &'static str {
            "test echo"
        }

        fn schema(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: arguments.to_string(),
                success: true,
            })
        }
    }

    #[tokio::test]
    async fn rejects_duplicates_and_dispatches_by_name() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool).unwrap();
        assert!(registry.register(EchoTool).is_err());

        let output = registry
            .execute(&ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: json!({"value": 7}),
            })
            .await
            .unwrap();
        assert_eq!(output.content, r#"{"value":7}"#);
        assert!(
            registry
                .execute(&ToolCall {
                    id: "2".into(),
                    name: "missing".into(),
                    arguments: json!({}),
                })
                .await
                .is_err()
        );
    }
}
