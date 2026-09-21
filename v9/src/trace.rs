use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use uuid::Uuid;
const MAX_ERROR_CHARS: usize = 2_048;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "actor", rename_all = "snake_case")]
pub enum TraceActor {
    Parent,
    Child {
        child_id: Uuid,
        parent_tool_call_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub timestamp: DateTime<Utc>,
    pub session_id: Uuid,
    #[serde(flatten)]
    pub actor: TraceActor,
    #[serde(flatten)]
    pub kind: TraceEventKind,
}
impl TraceEvent {
    pub fn new(session_id: Uuid, kind: TraceEventKind) -> Self {
        Self {
            timestamp: Utc::now(),
            session_id,
            actor: TraceActor::Parent,
            kind,
        }
    }

    pub fn for_child(
        session_id: Uuid,
        child_id: Uuid,
        parent_tool_call_id: impl Into<String>,
        kind: TraceEventKind,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            session_id,
            actor: TraceActor::Child {
                child_id,
                parent_tool_call_id: parent_tool_call_id.into(),
            },
            kind,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEventKind {
    SessionStarted,
    SessionResumed,
    McpServerConnected {
        server: String,
        tool_count: usize,
    },
    McpToolRegistered {
        server: String,
        name: String,
    },
    McpCallResult {
        server: String,
        name: String,
        success: bool,
    },
    ContextBuilt {
        step: usize,
        token_budget: usize,
        estimated_tokens: usize,
        original_messages: usize,
        retained_messages: usize,
        omitted_messages: usize,
        omitted_groups: usize,
        compacted: bool,
    },
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
        original_chars: usize,
        retained_chars: usize,
        truncated: bool,
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
    SubAgentStarted {
        child_id: Uuid,
        parent_tool_call_id: String,
        role: String,
        steps: usize,
        status: String,
    },
    SubAgentCompleted {
        child_id: Uuid,
        parent_tool_call_id: String,
        role: String,
        steps: usize,
        status: String,
    },
    SubAgentFailed {
        child_id: Uuid,
        parent_tool_call_id: String,
        role: String,
        steps: usize,
        status: String,
    },
    ToolOutcomeIndeterminate {
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
    AgentAborted {
        step: usize,
    },
    MaxStepsReached {
        step: usize,
    },
}
pub trait TraceSink: Send + Sync {
    fn record(&self, event: &TraceEvent) -> Result<()>;
}
pub struct JsonlTraceWriter {
    session_id: Uuid,
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}
impl JsonlTraceWriter {
    pub fn create(workspace: &Path, session_id: Uuid) -> Result<Self> {
        let directory = checked_trace_directory(workspace, true)?;
        let path = trace_path(&directory, session_id);
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
    pub fn open_append(workspace: &Path, session_id: Uuid) -> Result<Self> {
        let path = resolve_trace_path(workspace, session_id)?;
        for (index, line) in
            BufReader::new(File::open(&path).context("failed to open existing trace")?)
                .lines()
                .enumerate()
        {
            let line = line.context("failed to read existing trace")?;
            if line.is_empty() {
                bail!("trace contains an empty line at {}", index + 1);
            }
            let event: TraceEvent = serde_json::from_str(&line)
                .with_context(|| format!("trace contains invalid JSON at line {}", index + 1))?;
            if event.session_id != session_id {
                bail!("trace contains an event for a different session");
            }
        }
        let mut options = OpenOptions::new();
        options.append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .context("failed to reopen trace for append")?;
        if !file
            .metadata()
            .context("failed to inspect opened trace")?
            .is_file()
        {
            bail!("trace path must be a regular file");
        }
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
pub fn trace_path(directory: &Path, id: Uuid) -> PathBuf {
    directory.join(format!("{id}.jsonl"))
}
pub fn resolve_trace_path(workspace: &Path, id: Uuid) -> Result<PathBuf> {
    let directory = checked_trace_directory(workspace, false)?;
    let expected = trace_path(&directory, id);
    let metadata =
        fs::symlink_metadata(&expected).with_context(|| format!("trace `{id}` does not exist"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("trace path must be a regular file, not a symlink");
    }
    let canonical =
        fs::canonicalize(&expected).with_context(|| format!("trace `{id}` does not exist"))?;
    if canonical.parent() != Some(directory.as_path()) {
        bail!("trace path resolves outside the trace directory");
    }
    Ok(canonical)
}
fn checked_trace_directory(workspace: &Path, create: bool) -> Result<PathBuf> {
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    let directory = workspace.join("traces");
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(&directory).context("failed to create trace directory")?;
            fs::symlink_metadata(&directory).context("failed to inspect trace directory")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("trace directory `{}` does not exist", directory.display())
        }
        Err(e) => return Err(e).context("failed to inspect trace directory"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("trace path must be a real directory inside the workspace");
    }
    let canonical = fs::canonicalize(&directory).context("failed to resolve trace directory")?;
    if canonical.parent() != Some(workspace.as_path()) {
        bail!("trace directory resolves outside the workspace");
    }
    Ok(canonical)
}
pub fn parse_session_id(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value).context("session ID must be a valid UUID")?;
    if value != id.hyphenated().to_string() {
        bail!("session ID must use canonical hyphenated UUID format");
    }
    Ok(id)
}
pub fn safe_error(error: &str) -> String {
    let redacted = error
        .lines()
        .map(|line| {
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
                "[REDACTED]".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut bounded = redacted.chars().take(MAX_ERROR_CHARS).collect::<String>();
    if redacted.chars().count() > MAX_ERROR_CHARS {
        bounded.push_str("…[truncated]");
    }
    bounded
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    #[test]
    fn writer_flushes_metadata_without_content() {
        let w = TestWorkspace::new("trace");
        let id = Uuid::from_u128(42);
        let writer = JsonlTraceWriter::create(w.path(), id).unwrap();
        writer
            .record(&TraceEvent::new(
                id,
                TraceEventKind::ContextBuilt {
                    step: 1,
                    token_budget: 100,
                    estimated_tokens: 80,
                    original_messages: 10,
                    retained_messages: 4,
                    omitted_messages: 7,
                    omitted_groups: 3,
                    compacted: true,
                },
            ))
            .unwrap();
        let text = fs::read_to_string(writer.path()).unwrap();
        assert!(text.contains("estimated_tokens") && !text.contains("message content"));
        serde_json::from_str::<TraceEvent>(text.trim()).unwrap();
    }
    #[test]
    fn canonical_uuid_validation_prevents_path_traversal() {
        let id = Uuid::from_u128(7);
        assert_eq!(parse_session_id(&id.to_string()).unwrap(), id);
        assert!(parse_session_id("../trace").is_err());
    }
    #[test]
    fn mcp_trace_events_contain_metadata_without_payloads() {
        let event = TraceEvent::new(
            Uuid::nil(),
            TraceEventKind::McpCallResult {
                server: "local".into(),
                name: "mcp__local__echo".into(),
                success: true,
            },
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("mcp_call_result"));
        for forbidden in [
            "arguments",
            "structuredContent",
            "api_key",
            "result payload",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }
    #[test]
    fn safe_errors_are_redacted() {
        assert_eq!(safe_error("authorization: value"), "[REDACTED]");
    }
}

#[cfg(test)]
mod append_regression_tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn append_mode_validates_and_extends_same_trace() {
        let workspace = TestWorkspace::new("trace-append");
        let id = Uuid::from_u128(51);
        let writer = JsonlTraceWriter::create(workspace.path(), id).unwrap();
        writer
            .record(&TraceEvent::new(id, TraceEventKind::SessionStarted))
            .unwrap();
        drop(writer);
        let writer = JsonlTraceWriter::open_append(workspace.path(), id).unwrap();
        writer
            .record(&TraceEvent::new(id, TraceEventKind::SessionResumed))
            .unwrap();
        assert_eq!(
            fs::read_to_string(writer.path()).unwrap().lines().count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_trace_file() {
        use std::os::unix::fs::symlink;
        let workspace = TestWorkspace::new("trace-link");
        let directory = workspace.path().join("traces");
        fs::create_dir(&directory).unwrap();
        let outside = workspace.path().join("outside");
        fs::write(&outside, "{}\n").unwrap();
        let id = Uuid::from_u128(52);
        symlink(outside, trace_path(&directory, id)).unwrap();
        assert!(resolve_trace_path(workspace.path(), id).is_err());
        assert!(JsonlTraceWriter::open_append(workspace.path(), id).is_err());
    }
}
