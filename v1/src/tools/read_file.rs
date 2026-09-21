use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Tool, ToolOutput,
    tool::{canonical_workspace, resolve_path_argument},
};

/// Reads UTF-8 text files located inside the configured workspace.
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
        "Read a UTF-8 text file inside the workspace. The path must be relative to the workspace."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a UTF-8 text file, relative to the workspace"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let path = resolve_path_argument(&self.workspace, &arguments)?;
        if path.is_dir() {
            bail!("path is a directory, not a file: {}", path.display());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 file `{}`", path.display()))?;

        Ok(ToolOutput {
            content,
            success: true,
        })
    }
}
