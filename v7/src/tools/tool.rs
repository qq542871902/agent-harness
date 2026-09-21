use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_FILE_BYTES: usize = 1_048_576;
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 1_000;
pub(crate) const MAX_DIRECTORY_OUTPUT_BYTES: usize = 262_144;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub success: bool,
}

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
    let resolved = workspace
        .join(validate_relative_path(raw_path)?)
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

pub(crate) fn ensure_not_sensitive(workspace: &Path, resolved: &Path) -> Result<()> {
    let relative = resolved
        .strip_prefix(workspace)
        .context("resolved path escaped the workspace")?;
    if is_sensitive_relative_path(relative) {
        bail!("access to sensitive workspace paths is not allowed");
    }
    Ok(())
}

pub(crate) fn is_sensitive_relative_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(component) = component else {
            return false;
        };
        let lower = component.to_string_lossy().to_ascii_lowercase();
        (lower.starts_with(".env") && lower != ".env.example")
            || matches!(
                lower.as_str(),
                ".sessions"
                    | "traces"
                    | ".ssh"
                    | ".gnupg"
                    | ".aws"
                    | ".azure"
                    | ".kube"
                    | ".docker"
                    | "gcloud"
                    | "key"
                    | "keys"
                    | "keyring"
                    | "keyrings"
                    | "credential"
                    | "credentials"
                    | "secret"
                    | "secrets"
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_sensitive_paths_but_allows_env_example() {
        for path in [
            ".env",
            ".env.local",
            ".sessions/id.json",
            "traces/id.jsonl",
            ".ssh/id_ed25519",
            "config/credentials/token",
            "keys/signing.pem",
        ] {
            assert!(is_sensitive_relative_path(Path::new(path)), "{path}");
        }
        assert!(!is_sensitive_relative_path(Path::new(".env.example")));
        assert!(!is_sensitive_relative_path(Path::new("src/key.rs")));
    }
}
