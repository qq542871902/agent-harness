use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

pub(crate) const MAX_FILE_BYTES: usize = 1_048_576;
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 1_000;
pub(crate) const MAX_DIRECTORY_OUTPUT_BYTES: usize = 262_144;

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
    if is_sensitive_path(requested) {
        bail!("access to a sensitive workspace path is not allowed: {raw_path}");
    }
    let resolved = workspace
        .join(requested)
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace path `{raw_path}`"))?;
    ensure_confined(workspace, &resolved, raw_path)?;
    Ok(resolved)
}

pub(crate) fn ensure_confined(workspace: &Path, resolved: &Path, raw_path: &str) -> Result<()> {
    if !resolved.starts_with(workspace) {
        bail!("path escapes the workspace: {raw_path}");
    }
    Ok(())
}

/// Returns true when any component of a workspace-relative path names a
/// credential-bearing or harness-private file that must never be exposed to
/// the model (for example `.env`, private keys, or persisted session/trace state).
pub(crate) fn is_sensitive_path(relative: &Path) -> bool {
    relative.components().any(|component| match component {
        Component::Normal(name) => is_sensitive_name(&name.to_string_lossy()),
        _ => false,
    })
}

fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == ".env.example" {
        return false;
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    if matches!(
        lower.as_str(),
        ".sessions"
            | "traces"
            | "credentials"
            | "secrets"
            | ".ssh"
            | ".aws"
            | ".gcp"
            | ".azure"
            | "id_rsa"
            | "id_ed25519"
    ) {
        return true;
    }
    matches!(
        Path::new(&lower).extension().and_then(|ext| ext.to_str()),
        Some("pem" | "key" | "p12" | "pfx")
    )
}
