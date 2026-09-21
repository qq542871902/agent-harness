use super::{
    Tool, ToolOutput,
    tool::{MAX_FILE_BYTES, canonical_workspace, ensure_confined, validate_relative_path},
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArguments {
    path: String,
    content: String,
}
pub struct WriteFileTool {
    workspace: PathBuf,
    reserved_subtrees: Vec<PathBuf>,
}
impl WriteFileTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            workspace: canonical_workspace(workspace)?,
            reserved_subtrees: Vec::new(),
        })
    }
    pub fn with_reserved_subtree(mut self, raw_path: &str) -> Result<Self> {
        self.reserved_subtrees
            .push(self.workspace.join(validate_relative_path(raw_path)?));
        Ok(self)
    }
    fn resolve_target(&self, raw_path: &str) -> Result<PathBuf> {
        let requested = validate_relative_path(raw_path)?;
        let file_name = requested
            .file_name()
            .context("write_file path must name a file")?
            .to_owned();
        let joined = self.workspace.join(requested);
        if self
            .reserved_subtrees
            .iter()
            .any(|reserved| joined.starts_with(reserved))
        {
            bail!("write_file path is reserved by the harness: {raw_path}");
        }
        match fs::symlink_metadata(&joined) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!("write_file refuses symlink targets: {raw_path}");
                }
                if !metadata.is_file() {
                    bail!("write_file target is not a regular file: {raw_path}");
                }
                let target = joined
                    .canonicalize()
                    .with_context(|| format!("failed to resolve workspace path `{raw_path}`"))?;
                ensure_confined(&self.workspace, &target, raw_path)?;
                Ok(target)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = joined
                    .parent()
                    .context("write_file path must have a parent directory")?
                    .canonicalize()
                    .with_context(|| {
                        format!("failed to resolve parent directory for `{raw_path}`")
                    })?;
                ensure_confined(&self.workspace, &parent, raw_path)?;
                if !parent.is_dir() {
                    bail!("write_file parent is not a directory: {}", parent.display());
                }
                Ok(parent.join(file_name))
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect write target `{raw_path}`"))
            }
        }
    }
    fn atomic_write(&self, target: &Path, content: &[u8]) -> Result<()> {
        let parent = target.parent().context("write target has no parent")?;
        let existing = match fs::metadata(target) {
            Ok(m) => Some(m.permissions()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("failed to inspect existing permissions"),
        };
        let mut attempts = 0;
        let (temp_path, mut temp_file) = loop {
            if attempts == 100 {
                bail!("failed to allocate a temporary file for atomic write");
            }
            attempts += 1;
            let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".mini-harness-write-{}-{id}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("failed to create atomic write temp file"),
            }
        };
        let result = (|| -> Result<()> {
            if let Some(permissions) = existing {
                fs::set_permissions(&temp_path, permissions)
                    .context("failed to preserve existing file permissions")?;
            }
            temp_file
                .write_all(content)
                .context("failed to write temporary file")?;
            temp_file
                .sync_all()
                .context("failed to sync temporary file")?;
            drop(temp_file);
            if let Ok(metadata) = fs::symlink_metadata(target) {
                if metadata.file_type().is_symlink() {
                    bail!("write_file refuses a target that became a symlink");
                }
                if !metadata.is_file() {
                    bail!("write_file target is not a regular file");
                }
            }
            fs::rename(&temp_path, target).with_context(|| {
                format!("failed to replace write target `{}`", target.display())
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }
}
#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Safely create or replace a bounded UTF-8 regular file inside the workspace. The relative path cannot contain traversal, and symlink targets are rejected."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string","description":"File path relative to the workspace"},"content":{"type":"string","description":"Complete UTF-8 file content"}},"required":["path","content"],"additionalProperties":false})
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: WriteFileArguments =
            serde_json::from_value(arguments).context("invalid arguments for write_file")?;
        if arguments.content.len() > MAX_FILE_BYTES {
            bail!("content exceeds the {MAX_FILE_BYTES}-byte write limit");
        }
        self.atomic_write(
            &self.resolve_target(&arguments.path)?,
            arguments.content.as_bytes(),
        )?;
        Ok(ToolOutput {
            content: format!(
                "Wrote {} bytes to {}",
                arguments.content.len(),
                arguments.path
            ),
            success: true,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    #[tokio::test]
    async fn creates_and_replaces_regular_files() {
        let w = TestWorkspace::new("write");
        fs::create_dir(w.path().join("src")).unwrap();
        let t = WriteFileTool::new(w.path()).unwrap();
        t.execute(json!({"path":"src/new.txt","content":"new"}))
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(w.path().join("src/new.txt")).unwrap(),
            "new"
        );
    }
    #[tokio::test]
    async fn rejects_unknown_args_traversal_missing_parent_and_large_content() {
        let w = TestWorkspace::new("write-reject");
        let t = WriteFileTool::new(w.path()).unwrap();
        assert!(
            t.execute(json!({"path":"x","content":"x","mode":0}))
                .await
                .is_err()
        );
        assert!(
            t.execute(json!({"path":"../x","content":"x"}))
                .await
                .is_err()
        );
        assert!(
            t.execute(json!({"path":"x","content":"x".repeat(MAX_FILE_BYTES+1)}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn reserved_harness_subtrees_cannot_be_replaced() {
        let w = TestWorkspace::new("write-reserved");
        fs::create_dir(w.path().join("traces")).unwrap();
        fs::write(w.path().join("traces/a"), "old").unwrap();
        let t = WriteFileTool::new(w.path())
            .unwrap()
            .with_reserved_subtree("traces")
            .unwrap();
        assert!(
            t.execute(json!({"path":"traces/a","content":"new"}))
                .await
                .is_err()
        );
    }
}
