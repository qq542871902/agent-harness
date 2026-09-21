use super::{client::McpSession, config::validate_component};
use crate::tools::{Tool, ToolOutput};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

const MAX_MODEL_TOOL_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
const MAX_SCHEMA_BYTES: usize = 64 * 1024;

pub fn namespaced_name(server: &str, tool: &str) -> Result<String> {
    validate_component(server, "server name")?;
    validate_component(tool, "tool name")?;
    let name = format!("mcp__{server}__{tool}");
    if name.len() > MAX_MODEL_TOOL_NAME_BYTES {
        bail!("namespaced MCP tool name exceeds {MAX_MODEL_TOOL_NAME_BYTES} bytes");
    }
    Ok(name)
}

pub fn parse_namespaced_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if tool.contains("__") || namespaced_name(server, tool).ok().as_deref() != Some(name) {
        return None;
    }
    Some((server, tool))
}

pub struct McpTool {
    session: Arc<McpSession>,
    remote_name: String,
    name: String,
    description: String,
    schema: Value,
}

impl McpTool {
    pub(crate) fn new(
        session: Arc<McpSession>,
        remote_name: String,
        description: String,
        schema: Value,
    ) -> Result<Self> {
        let name = namespaced_name(session.server_name(), &remote_name)?;
        if description.len() > MAX_DESCRIPTION_BYTES {
            bail!("MCP tool `{name}` description exceeds {MAX_DESCRIPTION_BYTES} bytes");
        }
        if !schema.is_object() {
            bail!("MCP tool `{name}` inputSchema must be a JSON object");
        }
        let schema_bytes = serde_json::to_vec(&schema)?.len();
        if schema_bytes > MAX_SCHEMA_BYTES {
            bail!("MCP tool `{name}` inputSchema exceeds {MAX_SCHEMA_BYTES} bytes");
        }
        Ok(Self {
            session,
            remote_name,
            name,
            description,
            schema,
        })
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        self.session.call_tool(&self.remote_name, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespacing_is_deterministic_and_rejects_ambiguous_names() {
        assert_eq!(
            namespaced_name("git", "status").unwrap(),
            "mcp__git__status"
        );
        assert_eq!(
            parse_namespaced_name("mcp__git__status"),
            Some(("git", "status"))
        );
        assert!(namespaced_name("bad__server", "tool").is_err());
        assert!(parse_namespaced_name("mcp__git__bad__tool").is_none());
        assert!(parse_namespaced_name("mcp____tool").is_none());
    }
}
