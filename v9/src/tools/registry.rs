use super::{Tool, ToolOutput};
use crate::{
    llm::{FunctionDefinition, ToolCall, ToolDefinition, ToolDefinitionKind},
    mcp::parse_namespaced_name,
};
use anyhow::{Context, Result, bail};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<()> {
        self.register_arc(Arc::new(tool))
    }
    pub fn register_batch<T: Tool + 'static>(
        &mut self,
        tools: impl IntoIterator<Item = T>,
    ) -> Result<()> {
        let tools = tools
            .into_iter()
            .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
            .collect::<Vec<_>>();
        let mut pending = BTreeMap::new();
        for tool in tools {
            let name = tool.name().to_owned();
            if name.is_empty() {
                bail!("tool name must not be empty");
            }
            if self.tools.contains_key(&name) || pending.contains_key(&name) {
                bail!("tool `{name}` is already registered");
            }
            pending.insert(name, tool);
        }
        self.tools.extend(pending);
        Ok(())
    }
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.name();
        if name.is_empty() {
            bail!("tool name must not be empty");
        }
        if self.tools.contains_key(name) {
            bail!("tool `{name}` is already registered");
        }
        self.tools.insert(name.to_owned(), tool);
        Ok(())
    }
    pub fn filtered(&self, mut include: impl FnMut(&str) -> bool) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| include(name))
                .map(|(name, tool)| (name.clone(), Arc::clone(tool)))
                .collect(),
        }
    }
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                kind: ToolDefinitionKind::Function,
                function: FunctionDefinition {
                    name: tool.name().to_owned(),
                    description: tool.description().to_owned(),
                    parameters: tool.schema(),
                },
            })
            .collect()
    }
    pub fn mcp_tool_names(&self) -> Vec<String> {
        self.tools
            .keys()
            .filter(|name| parse_namespaced_name(name).is_some())
            .cloned()
            .collect()
    }
    pub async fn execute(&self, call: &ToolCall) -> Result<ToolOutput> {
        self.tools
            .get(&call.name)
            .with_context(|| format!("unknown tool `{}`", call.name))?
            .execute_for_call(&call.id, call.arguments.clone())
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
    async fn supports_owned_names_rejects_collisions_dispatches_and_filters() {
        let mut registry = ToolRegistry::new();
        registry
            .register(EchoTool {
                name: "echo".into(),
            })
            .unwrap();
        registry
            .register(EchoTool {
                name: "other".into(),
            })
            .unwrap();
        assert!(
            registry
                .register(EchoTool {
                    name: "echo".into()
                })
                .is_err()
        );
        let filtered = registry.filtered(|name| name == "echo");
        assert_eq!(filtered.names(), vec!["echo"]);
        assert_eq!(registry.names(), vec!["echo", "other"]);
        let output = filtered
            .execute(&ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: json!({"value":7}),
            })
            .await
            .unwrap();
        assert_eq!(output.content, r#"{"value":7}"#);
    }
}
