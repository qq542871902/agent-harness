use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Tool, ToolOutput,
    tool::{MAX_FILE_BYTES, canonical_workspace, resolve_existing_path},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArguments {
    path: String,
}

/// Reads bounded UTF-8 text files located inside the configured workspace.
pub struct ReadFileTool {
    workspace: std::path::PathBuf,
}

impl ReadFileTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            workspace: canonical_workspace(workspace)?,
        })
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a bounded UTF-8 regular file inside the workspace. The path must be relative and cannot contain traversal."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a UTF-8 regular file, relative to the workspace"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: ReadFileArguments =
            serde_json::from_value(arguments).context("invalid arguments for read_file")?;
        let path = resolve_existing_path(&self.workspace, &arguments.path)?;
        let metadata = path
            .metadata()
            .with_context(|| format!("failed to inspect `{}`", path.display()))?;
        if !metadata.is_file() {
            bail!("path is not a regular file: {}", path.display());
        }
        if metadata.len() > MAX_FILE_BYTES as u64 {
            bail!("file exceeds the {MAX_FILE_BYTES}-byte read limit");
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&path)
            .with_context(|| format!("failed to open `{}`", path.display()))?
            .take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read `{}`", path.display()))?;
        if bytes.len() > MAX_FILE_BYTES {
            bail!("file exceeds the {MAX_FILE_BYTES}-byte read limit");
        }
        let content = String::from_utf8(bytes)
            .with_context(|| format!("file is not valid UTF-8: {}", path.display()))?;

        Ok(ToolOutput {
            content,
            success: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::test_support::TestWorkspace;

    #[tokio::test]
    async fn rejects_unknown_arguments_and_traversal() {
        let workspace = TestWorkspace::new("read-strict");
        let tool = ReadFileTool::new(workspace.path()).unwrap();

        assert!(
            tool.execute(json!({"path": ".", "extra": true}))
                .await
                .is_err()
        );
        assert!(tool.execute(json!({"path": "../outside"})).await.is_err());
        assert!(
            tool.execute(json!({"path": workspace.path()}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reads_regular_file_and_rejects_oversized_input() {
        let workspace = TestWorkspace::new("read-bounds");
        fs::write(workspace.path().join("small.txt"), "hello").unwrap();
        fs::write(
            workspace.path().join("large.txt"),
            vec![b'x'; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        let tool = ReadFileTool::new(workspace.path()).unwrap();

        assert_eq!(
            tool.execute(json!({"path": "small.txt"}))
                .await
                .unwrap()
                .content,
            "hello"
        );
        assert!(tool.execute(json!({"path": "large.txt"})).await.is_err());
        assert!(tool.execute(json!({"path": "."})).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("read-link");
        symlink(std::env::temp_dir(), workspace.path().join("outside")).unwrap();
        let tool = ReadFileTool::new(workspace.path()).unwrap();

        assert!(tool.execute(json!({"path": "outside"})).await.is_err());
    }

    #[tokio::test]
    async fn denies_reading_sensitive_files() {
        let workspace = TestWorkspace::new("read-sensitive");
        fs::write(workspace.path().join(".env"), "OPENAI_API_KEY=secret").unwrap();
        fs::write(workspace.path().join(".env.example"), "OPENAI_API_KEY=").unwrap();
        let tool = ReadFileTool::new(workspace.path()).unwrap();

        assert!(tool.execute(json!({"path": ".env"})).await.is_err());
        // The non-secret template remains readable.
        assert_eq!(
            tool.execute(json!({"path": ".env.example"}))
                .await
                .unwrap()
                .content,
            "OPENAI_API_KEY="
        );
    }
}
