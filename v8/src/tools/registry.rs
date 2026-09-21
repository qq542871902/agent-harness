use super::{Tool, ToolOutput};
use crate::{
    llm::{FunctionDefinition, ToolCall, ToolDefinition, ToolDefinitionKind},
    mcp::parse_namespaced_name,
};
use anyhow::{Context, Result, bail};
use std::{collections::HashMap, sync::Arc};

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<()> {
        let name = tool.name();
        if name.is_empty() {
            bail!("tool name must not be empty");
        }
        if self.tools.contains_key(name) {
            bail!("tool `{name}` is already registered");
        }
        self.tools.insert(name.to_owned(), Arc::new(tool));
        Ok(())
    }
    pub(crate) fn register_batch<T: Tool + 'static>(&mut self, tools: Vec<T>) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            let name = tool.name();
            if name.is_empty() {
                bail!("tool name must not be empty");
            }
            if self.tools.contains_key(name) || !names.insert(name.to_owned()) {
                bail!("tool `{name}` is already registered");
            }
        }
        for tool in tools {
            self.tools.insert(tool.name().to_owned(), Arc::new(tool));
        }
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
        definitions.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        definitions
    }
    pub fn mcp_tool_names(&self) -> Vec<String> {
        let mut names = self
            .tools
            .keys()
            .filter(|name| parse_namespaced_name(name).is_some())
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
    }
    pub async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        self.tools
            .get(&call.name)
            .with_context(|| format!("unknown tool `{}`", call.name))?
            .execute(call.arguments.clone())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    struct EchoTool {
        name: String,
    }
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test echo"
        }
        fn schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn execute(&self, a: Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: a.to_string(),
                success: true,
            })
        }
    }
    #[tokio::test]
    async fn supports_owned_names_rejects_collisions_and_dispatches() {
        let mut registry = ToolRegistry::new();
        registry
            .register(EchoTool {
                name: "echo".into(),
            })
            .unwrap();
        assert!(
            registry
                .register(EchoTool {
                    name: "echo".into()
                })
                .is_err()
        );
        let output = registry
            .execute(&ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: json!({"value":7}),
            })
            .await
            .unwrap();
        assert_eq!(output.content, r#"{"value":7}"#);
        assert!(
            registry
                .execute(&ToolCall {
                    id: "2".into(),
                    name: "missing".into(),
                    arguments: json!({})
                })
                .await
                .is_err()
        );
    }
}
