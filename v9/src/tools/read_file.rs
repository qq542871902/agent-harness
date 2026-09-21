use super::{
    Tool, ToolOutput,
    tool::{MAX_FILE_BYTES, canonical_workspace, resolve_existing_path},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs::File, io::Read, path::Path};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArguments {
    path: String,
}
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
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a bounded UTF-8 regular file inside the workspace. The path must be relative and cannot contain traversal."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"Path to a UTF-8 regular file, relative to the workspace"}},"required":["path"],"additionalProperties":false})
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
        Ok(ToolOutput {
            content: String::from_utf8(bytes)
                .with_context(|| format!("file is not valid UTF-8: {}", path.display()))?,
            success: true,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    use std::fs;
    #[tokio::test]
    async fn rejects_unknown_arguments_and_traversal() {
        let w = TestWorkspace::new("read-strict");
        let t = ReadFileTool::new(w.path()).unwrap();
        assert!(t.execute(json!({"path":".","extra":true})).await.is_err());
        assert!(t.execute(json!({"path":"../outside"})).await.is_err());
    }
    #[tokio::test]
    async fn reads_regular_file_and_rejects_oversized_input() {
        let w = TestWorkspace::new("read-bounds");
        fs::write(w.path().join("small.txt"), "hello").unwrap();
        fs::write(w.path().join("large.txt"), vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        let t = ReadFileTool::new(w.path()).unwrap();
        assert_eq!(
            t.execute(json!({"path":"small.txt"}))
                .await
                .unwrap()
                .content,
            "hello"
        );
        assert!(t.execute(json!({"path":"large.txt"})).await.is_err());
    }
}

#[cfg(test)]
mod sensitive_path_tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    use serde_json::json;
    use std::fs;

    #[tokio::test]
    async fn rejects_sensitive_files_but_allows_the_example() {
        let w = TestWorkspace::new("read-sensitive");
        for path in [
            ".env",
            ".env.local",
            "private.key",
            "credentials.json",
            ".sessions/state.json",
            "traces/run.jsonl",
        ] {
            let path = w.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "secret").unwrap();
        }
        fs::write(w.path().join(".env.example"), "safe template").unwrap();
        let tool = ReadFileTool::new(w.path()).unwrap();
        for path in [
            ".env",
            ".env.local",
            "private.key",
            "credentials.json",
            ".sessions/state.json",
            "traces/run.jsonl",
        ] {
            assert!(tool.execute(json!({"path":path})).await.is_err(), "{path}");
        }
        assert_eq!(
            tool.execute(json!({"path":".env.example"}))
                .await
                .unwrap()
                .content,
            "safe template"
        );
    }
}
