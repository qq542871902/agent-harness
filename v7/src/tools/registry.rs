use super::{Tool, ToolOutput};
use crate::llm::{FunctionDefinition, ToolCall, ToolDefinition, ToolDefinitionKind};
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
        definitions.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        definitions
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
    async fn rejects_duplicates_and_dispatches_by_name() {
        let mut r = ToolRegistry::new();
        r.register(EchoTool).unwrap();
        assert!(r.register(EchoTool).is_err());
        let o = r
            .execute(&ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: json!({"value":7}),
            })
            .await
            .unwrap();
        assert_eq!(o.content, r#"{"value":7}"#);
        assert!(
            r.execute(&ToolCall {
                id: "2".into(),
                name: "missing".into(),
                arguments: json!({})
            })
            .await
            .is_err()
        );
    }
}
