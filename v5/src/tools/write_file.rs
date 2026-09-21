use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Tool, ToolOutput,
    tool::{MAX_FILE_BYTES, canonical_workspace, ensure_confined, validate_relative_path},
};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArguments {
    path: String,
    content: String,
}

/// Atomically writes bounded UTF-8 content to a regular file inside the workspace.
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

    /// Adds a workspace-relative subtree that model-requested writes cannot mutate.
    pub fn with_reserved_subtree(mut self, raw_path: &str) -> Result<Self> {
        let relative = validate_relative_path(raw_path)?;
        self.reserved_subtrees.push(self.workspace.join(relative));
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
        let existing_permissions = match fs::metadata(target) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("failed to inspect existing permissions"),
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
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("failed to create atomic write temp file"),
            }
        };

        let result = (|| -> Result<()> {
            if let Some(permissions) = existing_permissions {
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
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Safely create or replace a bounded UTF-8 regular file inside the workspace. The relative path cannot contain traversal, and symlink targets are rejected."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to the workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Complete UTF-8 file content"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: WriteFileArguments =
            serde_json::from_value(arguments).context("invalid arguments for write_file")?;
        if arguments.content.len() > MAX_FILE_BYTES {
            bail!("content exceeds the {MAX_FILE_BYTES}-byte write limit");
        }
        let target = self.resolve_target(&arguments.path)?;
        self.atomic_write(&target, arguments.content.as_bytes())?;

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
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::test_support::TestWorkspace;

    #[tokio::test]
    async fn creates_and_replaces_regular_files() {
        let workspace = TestWorkspace::new("write");
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("existing.txt"), "old").unwrap();
        let tool = WriteFileTool::new(workspace.path()).unwrap();

        tool.execute(json!({"path": "src/new.txt", "content": "new"}))
            .await
            .unwrap();
        tool.execute(json!({"path": "existing.txt", "content": "updated"}))
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(workspace.path().join("src/new.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("existing.txt")).unwrap(),
            "updated"
        );
    }

    #[tokio::test]
    async fn rejects_unknown_args_traversal_missing_parent_and_large_content() {
        let workspace = TestWorkspace::new("write-reject");
        let tool = WriteFileTool::new(workspace.path()).unwrap();

        assert!(
            tool.execute(json!({"path": "x", "content": "x", "mode": 0}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"path": "../x", "content": "x"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"path": "missing/x", "content": "x"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"path": "x", "content": "x".repeat(MAX_FILE_BYTES + 1)}))
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_permissions_when_replacing_a_file() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TestWorkspace::new("write-mode");
        let path = workspace.path().join("script.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        let tool = WriteFileTool::new(workspace.path()).unwrap();

        tool.execute(json!({"path": "script.sh", "content": "new"}))
            .await
            .unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_targets_and_symlink_parent_escape() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("write-link");
        let outside = TestWorkspace::new("outside");
        fs::write(outside.path().join("target.txt"), "outside").unwrap();
        symlink(
            outside.path().join("target.txt"),
            workspace.path().join("target-link"),
        )
        .unwrap();
        symlink(outside.path(), workspace.path().join("dir-link")).unwrap();
        let tool = WriteFileTool::new(workspace.path()).unwrap();

        assert!(
            tool.execute(json!({"path": "target-link", "content": "bad"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"path": "dir-link/new", "content": "bad"}))
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("target.txt")).unwrap(),
            "outside"
        );
    }

    #[tokio::test]
    async fn reserved_trace_subtree_cannot_be_replaced() {
        let workspace = TestWorkspace::new("write-reserved");
        let trace_directory = workspace.path().join("traces");
        fs::create_dir(&trace_directory).unwrap();
        let trace = trace_directory.join("active.jsonl");
        fs::write(&trace, "original\n").unwrap();
        let tool = WriteFileTool::new(workspace.path())
            .unwrap()
            .with_reserved_subtree("traces")
            .unwrap();

        assert!(
            tool.execute(json!({"path": "traces/active.jsonl", "content": "replaced"}))
                .await
                .is_err()
        );
        assert_eq!(fs::read_to_string(trace).unwrap(), "original\n");
    }
}
