use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

/// The output produced by a native tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub success: bool,
}

/// Common interface for every tool registered with the harness.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    async fn execute(&self, arguments: Value) -> Result<ToolOutput>;
}

pub(crate) fn canonical_workspace(workspace: impl AsRef<Path>) -> Result<PathBuf> {
    workspace.as_ref().canonicalize().with_context(|| {
        format!(
            "failed to resolve workspace `{}`",
            workspace.as_ref().display()
        )
    })
}

pub(crate) fn resolve_path_argument(workspace: &Path, arguments: &Value) -> Result<PathBuf> {
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .context("tool argument `path` must be a non-empty string")?;
    let requested = Path::new(raw_path);

    if requested.is_absolute() {
        bail!("absolute paths are not allowed: {raw_path}");
    }

    let resolved = workspace
        .join(requested)
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace path `{raw_path}`"))?;

    if !resolved.starts_with(workspace) {
        bail!("path escapes the workspace: {raw_path}");
    }

    Ok(resolved)
}
