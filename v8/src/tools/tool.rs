use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_FILE_BYTES: usize = 1_048_576;
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 1_000;
pub(crate) const MAX_DIRECTORY_OUTPUT_BYTES: usize = 262_144;

fn is_sensitive_relative_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return true;
        };
        matches!(name, ".git" | ".sessions" | "traces" | ".ssh" | ".aws")
            || name == ".env"
            || name.starts_with(".env.")
            || name.ends_with(".pem")
            || name.ends_with(".key")
    })
}

pub(crate) fn is_sensitive_workspace_path(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace)
        .map(is_sensitive_relative_path)
        .unwrap_or(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub success: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn execute(&self, arguments: Value) -> Result<ToolOutput>;
}

pub(crate) fn canonical_workspace(workspace: impl AsRef<Path>) -> Result<PathBuf> {
    let workspace = workspace.as_ref().canonicalize().with_context(|| {
        format!(
            "failed to resolve workspace `{}`",
            workspace.as_ref().display()
        )
    })?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    Ok(workspace)
}

pub(crate) fn validate_relative_path(raw_path: &str) -> Result<&Path> {
    if raw_path.trim().is_empty() {
        bail!("tool argument `path` must be a non-empty string");
    }
    let requested = Path::new(raw_path);
    if requested.is_absolute() {
        bail!("absolute paths are not allowed: {raw_path}");
    }
    for component in requested.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => bail!("path traversal is not allowed: {raw_path}"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("absolute paths are not allowed: {raw_path}")
            }
        }
    }
    Ok(requested)
}

pub(crate) fn resolve_existing_path(workspace: &Path, raw_path: &str) -> Result<PathBuf> {
    let requested = validate_relative_path(raw_path)?;
    if is_sensitive_relative_path(requested) {
        bail!("access to sensitive workspace path is denied: {raw_path}");
    }
    let resolved = workspace
        .join(requested)
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace path `{raw_path}`"))?;
    ensure_confined(workspace, &resolved, raw_path)?;
    if is_sensitive_workspace_path(workspace, &resolved) {
        bail!("access to sensitive workspace path is denied: {raw_path}");
    }
    Ok(resolved)
}

pub(crate) fn ensure_confined(workspace: &Path, resolved: &Path, raw_path: &str) -> Result<()> {
    if !resolved.starts_with(workspace) {
        bail!("path escapes the workspace: {raw_path}");
    }
    Ok(())
}
