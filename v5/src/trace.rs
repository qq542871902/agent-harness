use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_ERROR_CHARS: usize = 2_048;

/// One timestamped, session-scoped event in an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub timestamp: DateTime<Utc>,
    pub session_id: Uuid,
    #[serde(flatten)]
    pub kind: TraceEventKind,
}

impl TraceEvent {
    pub fn new(session_id: Uuid, kind: TraceEventKind) -> Self {
        Self {
            timestamp: Utc::now(),
            session_id,
            kind,
        }
    }
}

/// Structured events needed to reconstruct one complete run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEventKind {
    SessionStarted,
    ModelRequest {
        step: usize,
    },
    ModelResponse {
        step: usize,
        final_response: bool,
        tool_call_count: usize,
    },
    ToolCall {
        id: String,
        name: String,
    },
    ToolResult {
        id: String,
        name: String,
        success: bool,
    },
    ApprovalRequested {
        id: String,
        name: String,
    },
    ApprovalGranted {
        id: String,
        name: String,
    },
    ApprovalDenied {
        id: String,
        name: String,
    },
    AgentCompleted {
        step: usize,
    },
    AgentFailed {
        step: usize,
        error: String,
    },
    MaxStepsReached {
        step: usize,
    },
}

/// Observer port for run events. AgentRunner has no filesystem dependency.
pub trait TraceSink: Send + Sync {
    fn record(&self, event: &TraceEvent) -> Result<()>;
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NullTraceSink;

#[cfg(test)]
impl TraceSink for NullTraceSink {
    fn record(&self, _event: &TraceEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub static NULL_TRACE_SINK: NullTraceSink = NullTraceSink;

/// Flush-on-event JSONL adapter. Each instance owns exactly one session file.
pub struct JsonlTraceWriter {
    session_id: Uuid,
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}

impl JsonlTraceWriter {
    /// Creates `traces/{session_id}.jsonl` beneath a canonical workspace root.
    pub fn create(workspace: &Path, session_id: Uuid) -> Result<Self> {
        let trace_directory = checked_trace_directory(workspace, true)?;
        let path = trace_path(&trace_directory, session_id);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create trace file `{}`", path.display()))?;
        Ok(Self {
            session_id,
            path,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TraceSink for JsonlTraceWriter {
    fn record(&self, event: &TraceEvent) -> Result<()> {
        if event.session_id != self.session_id {
            bail!("trace event session does not match writer session");
        }
        let mut line = serde_json::to_vec(event).context("failed to serialize trace event")?;
        line.push(b'\n');
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("trace writer lock was poisoned"))?;
        writer
            .write_all(&line)
            .context("failed to write trace event")?;
        writer.flush().context("failed to flush trace event")
    }
}

pub fn trace_path(trace_directory: &Path, session_id: Uuid) -> PathBuf {
    trace_directory.join(format!("{session_id}.jsonl"))
}

/// Resolves an existing trace, anchored to the canonical workspace root.
pub fn resolve_trace_path(workspace: &Path, session_id: Uuid) -> Result<PathBuf> {
    let trace_directory = checked_trace_directory(workspace, false)?;
    let expected_path = trace_path(&trace_directory, session_id);
    let canonical_path = fs::canonicalize(&expected_path)
        .with_context(|| format!("trace `{session_id}` does not exist"))?;
    if canonical_path.parent() != Some(trace_directory.as_path()) {
        bail!("trace path resolves outside the trace directory");
    }
    Ok(canonical_path)
}

fn checked_trace_directory(workspace: &Path, create: bool) -> Result<PathBuf> {
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    let trace_directory = workspace.join("traces");
    let metadata = match fs::symlink_metadata(&trace_directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(&trace_directory).with_context(|| {
                format!(
                    "failed to create trace directory `{}`",
                    trace_directory.display()
                )
            })?;
            fs::symlink_metadata(&trace_directory)
                .context("failed to inspect newly created trace directory")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "trace directory `{}` does not exist",
                trace_directory.display()
            )
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect trace directory `{}`",
                    trace_directory.display()
                )
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "trace path must be a real directory inside the workspace: {}",
            trace_directory.display()
        );
    }
    let canonical_directory =
        fs::canonicalize(&trace_directory).context("failed to resolve trace directory")?;
    if canonical_directory.parent() != Some(workspace.as_path()) {
        bail!("trace directory resolves outside the workspace");
    }
    Ok(canonical_directory)
}

pub fn parse_session_id(value: &str) -> Result<Uuid> {
    let session_id = Uuid::parse_str(value).context("session ID must be a valid UUID")?;
    if value != session_id.hyphenated().to_string() {
        bail!("session ID must use canonical hyphenated UUID format");
    }
    Ok(session_id)
}

/// Bounds fatal diagnostics and removes lines likely to carry credentials.
pub fn safe_error(error: &str) -> String {
    let mut redacted = String::new();
    for (index, line) in error.lines().enumerate() {
        if index > 0 {
            redacted.push('\n');
        }
        let lower = line.to_ascii_lowercase();
        if [
            "api_key",
            "apikey",
            "authorization",
            "bearer ",
            "password",
            "secret",
            "access_token",
            "refresh_token",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            redacted.push_str("[REDACTED]");
        } else {
            redacted.push_str(line);
        }
    }

    let mut bounded: String = redacted.chars().take(MAX_ERROR_CHARS).collect();
    if redacted.chars().count() > MAX_ERROR_CHARS {
        bounded.push_str("…[truncated]");
    }
    bounded
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn writer_flushes_valid_jsonl_without_secret_arguments() {
        let workspace = TestWorkspace::new("trace-writer");
        let session_id = Uuid::from_u128(42);
        let writer = JsonlTraceWriter::create(workspace.path(), session_id).unwrap();
        writer
            .record(&TraceEvent::new(
                session_id,
                TraceEventKind::ToolCall {
                    id: "call-1".into(),
                    name: "write_file".into(),
                },
            ))
            .unwrap();
        writer
            .record(&TraceEvent::new(
                session_id,
                TraceEventKind::AgentFailed {
                    step: 1,
                    error: safe_error("authorization: super-secret"),
                },
            ))
            .unwrap();

        let contents = fs::read_to_string(writer.path()).unwrap();
        assert_eq!(contents.lines().count(), 2);
        for line in contents.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
        assert!(!contents.contains("super-secret"));
        assert!(contents.contains("[REDACTED]"));
    }

    #[test]
    fn canonical_uuid_validation_prevents_path_traversal() {
        let id = Uuid::from_u128(7);
        assert_eq!(parse_session_id(&id.to_string()).unwrap(), id);
        for invalid in ["../trace", "7", "{00000000-0000-0000-0000-000000000007}"] {
            assert!(parse_session_id(invalid).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn trace_resolution_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("trace-symlink");
        let trace_directory = workspace.path().join("traces");
        fs::create_dir(&trace_directory).unwrap();
        let outside = workspace.path().join("outside.jsonl");
        fs::write(&outside, "{}\n").unwrap();
        let id = Uuid::from_u128(8);
        symlink(&outside, trace_path(&trace_directory, id)).unwrap();

        assert!(resolve_trace_path(workspace.path(), id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn writer_and_reader_reject_symlinked_trace_directory() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("trace-root-symlink");
        let outside = TestWorkspace::new("trace-outside");
        let id = Uuid::from_u128(9);
        fs::write(trace_path(outside.path(), id), "{}\n").unwrap();
        symlink(outside.path(), workspace.path().join("traces")).unwrap();

        assert!(JsonlTraceWriter::create(workspace.path(), id).is_err());
        assert!(resolve_trace_path(workspace.path(), id).is_err());
    }
}
