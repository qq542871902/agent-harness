use super::{
    Tool, ToolOutput,
    tool::{
        MAX_DIRECTORY_ENTRIES, MAX_DIRECTORY_OUTPUT_BYTES, canonical_workspace,
        ensure_not_sensitive, is_sensitive_relative_path, resolve_existing_path,
    },
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, path::Path};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListFilesArguments {
    path: String,
}
pub struct ListFilesTool {
    workspace: std::path::PathBuf,
}
impl ListFilesTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            workspace: canonical_workspace(workspace)?,
        })
    }
}
#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &'static str {
        "list_files"
    }
    fn description(&self) -> &'static str {
        "List a bounded number of direct children of a directory inside the workspace. The path must be relative and cannot contain traversal."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"Directory path relative to the workspace; use . for the workspace root"}},"required":["path"],"additionalProperties":false})
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: ListFilesArguments =
            serde_json::from_value(arguments).context("invalid arguments for list_files")?;
        let directory = resolve_existing_path(&self.workspace, &arguments.path)?;
        ensure_not_sensitive(&self.workspace, &directory)?;
        if !directory
            .metadata()
            .with_context(|| format!("failed to inspect `{}`", directory.display()))?
            .is_dir()
        {
            bail!("path is not a directory: {}", directory.display());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to list directory `{}`", directory.display()))?
        {
            if entries.len() == MAX_DIRECTORY_ENTRIES {
                bail!("directory exceeds the {MAX_DIRECTORY_ENTRIES}-entry limit");
            }
            let path = entry.context("failed to read directory entry")?.path();
            let relative = path
                .strip_prefix(&self.workspace)
                .context("directory entry escaped the workspace")?;
            if is_sensitive_relative_path(relative) {
                continue;
            }
            entries.push(relative.display().to_string());
        }
        entries.sort();
        let content = entries.join("\n");
        if content.len() > MAX_DIRECTORY_OUTPUT_BYTES {
            bail!("directory listing exceeds the {MAX_DIRECTORY_OUTPUT_BYTES}-byte output limit");
        }
        Ok(ToolOutput {
            content,
            success: true,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    #[tokio::test]
    async fn lists_sorted_entries_with_strict_arguments() {
        let w = TestWorkspace::new("list");
        fs::write(w.path().join("b.txt"), "b").unwrap();
        fs::write(w.path().join("a.txt"), "a").unwrap();
        let t = ListFilesTool::new(w.path()).unwrap();
        assert_eq!(
            t.execute(json!({"path":"."})).await.unwrap().content,
            "a.txt\nb.txt"
        );
        assert!(
            t.execute(json!({"path":".","recursive":true}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn bounds_directory_entries() {
        let w = TestWorkspace::new("list-bound");
        for i in 0..=MAX_DIRECTORY_ENTRIES {
            fs::write(w.path().join(format!("{i:04}")), "").unwrap();
        }
        assert!(
            ListFilesTool::new(w.path())
                .unwrap()
                .execute(json!({"path":"."}))
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod sensitive_path_tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[tokio::test]
    async fn hides_sensitive_entries_and_denies_sensitive_directories() {
        let workspace = TestWorkspace::new("list-sensitive");
        fs::write(workspace.path().join("visible.txt"), "ok").unwrap();
        fs::write(workspace.path().join(".env"), "secret").unwrap();
        fs::write(workspace.path().join(".env.example"), "example").unwrap();
        fs::create_dir(workspace.path().join(".sessions")).unwrap();
        let tool = ListFilesTool::new(workspace.path()).unwrap();

        let output = tool.execute(json!({"path":"."})).await.unwrap().content;
        assert_eq!(output, ".env.example\nvisible.txt");
        assert!(tool.execute(json!({"path":".sessions"})).await.is_err());
    }
}
