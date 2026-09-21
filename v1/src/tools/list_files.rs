use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Tool, ToolOutput,
    tool::{canonical_workspace, resolve_path_argument},
};

/// Lists direct children of a directory located inside the configured workspace.
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
        "List the direct children of a directory inside the workspace. The path must be relative to the workspace."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to the workspace; use . for the workspace root"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let directory = resolve_path_argument(&self.workspace, &arguments)?;
        if !directory.is_dir() {
            bail!("path is not a directory: {}", directory.display());
        }

        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("failed to list directory `{}`", directory.display()))?
            .map(|entry| {
                let entry = entry.context("failed to read directory entry")?;
                let path = entry.path();
                let relative = path
                    .strip_prefix(&self.workspace)
                    .context("directory entry escaped the workspace")?;
                Ok(relative.display().to_string())
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort();

        Ok(ToolOutput {
            content: entries.join("\n"),
            success: true,
        })
    }
}
