use crate::{agent::AgentState, context::ContextSettings, mcp::parse_namespaced_name};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use uuid::Uuid;
pub const SESSION_SCHEMA_VERSION: u32 = 4;
const MAX_SESSION_BYTES: u64 = 4 * 1024 * 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub session_id: Uuid,
    pub workspace: PathBuf,
    pub model: String,
    pub context_settings: ContextSettings,
    /// Model-visible MCP tool names only. No command, environment, or secret values are persisted.
    pub mcp_tools: Vec<String>,
    pub state: AgentState,
    pub trace_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
impl Session {
    pub fn new(
        session_id: Uuid,
        workspace: &Path,
        model: impl Into<String>,
        context_settings: ContextSettings,
        mcp_tools: Vec<String>,
        state: AgentState,
        trace_path: PathBuf,
    ) -> Result<Self> {
        let workspace = canonical_workspace(workspace)?;
        let now = Utc::now();
        let session = Self {
            version: SESSION_SCHEMA_VERSION,
            session_id,
            workspace,
            model: model.into(),
            context_settings,
            mcp_tools,
            state,
            trace_path,
            created_at: now,
            updated_at: now,
        };
        validate_session(&session, session_id, &session.workspace)?;
        Ok(session)
    }
}
pub trait SessionRepository: Send + Sync {
    fn create(&self, session: &Session) -> Result<()>;
    fn save(&self, session: &Session) -> Result<()>;
    fn load(&self, id: Uuid) -> Result<Session>;
}
pub trait SessionSink: Send + Sync {
    fn checkpoint(&self, state: &AgentState) -> Result<()>;
}
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSessionSink;
#[cfg(test)]
impl SessionSink for NullSessionSink {
    fn checkpoint(&self, _: &AgentState) -> Result<()> {
        Ok(())
    }
}
#[cfg(test)]
pub static NULL_SESSION_SINK: NullSessionSink = NullSessionSink;
pub struct SessionCheckpoint<'a> {
    repository: &'a dyn SessionRepository,
    session: Mutex<Session>,
}
impl<'a> SessionCheckpoint<'a> {
    pub fn new(repository: &'a dyn SessionRepository, session: Session) -> Self {
        Self {
            repository,
            session: Mutex::new(session),
        }
    }
}
impl SessionSink for SessionCheckpoint<'_> {
    fn checkpoint(&self, state: &AgentState) -> Result<()> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("session checkpoint lock was poisoned"))?;
        session.state = state.clone();
        session.updated_at = Utc::now();
        self.repository.save(&session)
    }
}
pub struct SessionLease {
    #[cfg(not(unix))]
    path: PathBuf,
    _file: File,
}
impl Drop for SessionLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
#[derive(Debug, Clone)]
pub struct FileSessionRepository {
    workspace: PathBuf,
}
impl FileSessionRepository {
    pub fn new(workspace: &Path) -> Result<Self> {
        let workspace = canonical_workspace(workspace)?;
        checked_session_directory(&workspace, true)?;
        Ok(Self { workspace })
    }
    pub fn acquire(&self, id: Uuid) -> Result<SessionLease> {
        let path = checked_session_directory(&self.workspace, false)?.join(format!("{id}.lock"));
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            bail!("session lock path must be a regular file");
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).context("failed to open session lock")?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                bail!("session `{id}` is already active");
            }
        }
        #[cfg(not(unix))]
        {
            if file.metadata()?.len() != 0 {
                bail!("session `{id}` is already active");
            }
            file.set_len(1)?;
        }
        Ok(SessionLease {
            #[cfg(not(unix))]
            path,
            _file: file,
        })
    }
    #[cfg(test)]
    pub fn session_path(&self, id: Uuid) -> PathBuf {
        self.workspace.join(".sessions").join(format!("{id}.json"))
    }
    fn checked_path(&self, id: Uuid, must_exist: bool) -> Result<PathBuf> {
        let directory = checked_session_directory(&self.workspace, false)?;
        let path = directory.join(format!("{id}.json"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!("session path must be a regular file");
                }
                let canonical =
                    fs::canonicalize(&path).context("failed to resolve session file")?;
                if canonical.parent() != Some(directory.as_path()) {
                    bail!("session path resolves outside the session directory");
                }
                Ok(canonical)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("session `{id}` does not exist")
            }
            Err(e) => Err(e).context("failed to inspect session file"),
        }
    }
    fn encoded(&self, session: &Session) -> Result<Vec<u8>> {
        validate_session(session, session.session_id, &self.workspace)?;
        let bytes = serde_json::to_vec_pretty(session).context("failed to serialize session")?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            bail!("session exceeds the {MAX_SESSION_BYTES}-byte limit");
        }
        Ok(bytes)
    }
    fn write_temp(&self, bytes: &[u8]) -> Result<(PathBuf, File)> {
        let directory = checked_session_directory(&self.workspace, false)?;
        for _ in 0..100 {
            let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                ".mini-harness-session-{}-{id}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        file.set_permissions(fs::Permissions::from_mode(0o600))
                            .context("failed to protect temporary session file")?;
                    }
                    file.write_all(bytes).context("failed to write session")?;
                    file.sync_all().context("failed to sync session")?;
                    return Ok((path, file));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("failed to create session temp file"),
            }
        }
        bail!("failed to allocate a temporary session file")
    }
    fn sync_directory(&self) -> Result<()> {
        File::open(self.workspace.join(".sessions"))
            .and_then(|d| d.sync_all())
            .context("failed to sync session directory")
    }
}
impl SessionRepository for FileSessionRepository {
    fn create(&self, session: &Session) -> Result<()> {
        let target = self.checked_path(session.session_id, false)?;
        if fs::symlink_metadata(&target).is_ok() {
            bail!("session `{}` already exists", session.session_id);
        }
        let bytes = self.encoded(session)?;
        let (temp, file) = self.write_temp(&bytes)?;
        drop(file);
        let result = fs::hard_link(&temp, &target)
            .context("failed to create new session without replacing an existing session")
            .and_then(|()| self.sync_directory());
        let _ = fs::remove_file(temp);
        result
    }
    fn save(&self, session: &Session) -> Result<()> {
        let target = self.checked_path(session.session_id, true)?;
        let bytes = self.encoded(session)?;
        let (temp, file) = self.write_temp(&bytes)?;
        drop(file);
        let result = fs::rename(&temp, &target)
            .context("failed to atomically replace session")
            .and_then(|()| self.sync_directory());
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    fn load(&self, id: Uuid) -> Result<Session> {
        let path = self.checked_path(id, true)?;
        let mut file = File::open(path).context("failed to open session")?;
        let length = file.metadata().context("failed to inspect session")?.len();
        if length > MAX_SESSION_BYTES {
            bail!("session exceeds the {MAX_SESSION_BYTES}-byte limit");
        }
        let mut bytes = Vec::with_capacity(length as usize);
        std::io::Read::by_ref(&mut file)
            .take(MAX_SESSION_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("failed to read session")?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            bail!("session exceeds the {MAX_SESSION_BYTES}-byte limit");
        }
        #[derive(Deserialize)]
        struct Envelope {
            version: u32,
        }
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .context("session file contains invalid JSON or no schema version")?;
        if envelope.version != SESSION_SCHEMA_VERSION {
            bail!(
                "unsupported session schema version {}; V8 requires version {SESSION_SCHEMA_VERSION} and does not migrate older sessions",
                envelope.version
            );
        }
        let session: Session = serde_json::from_slice(&bytes)
            .context("session file contains invalid V8 session JSON")?;
        validate_session(&session, id, &self.workspace)?;
        Ok(session)
    }
}
fn canonical_workspace(workspace: &Path) -> Result<PathBuf> {
    let workspace = fs::canonicalize(workspace)
        .with_context(|| format!("failed to resolve workspace `{}`", workspace.display()))?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    Ok(workspace)
}
fn checked_session_directory(workspace: &Path, create: bool) -> Result<PathBuf> {
    let directory = workspace.join(".sessions");
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(&directory).context("failed to create session directory")?;
            fs::symlink_metadata(&directory).context("failed to inspect session directory")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("session directory does not exist")
        }
        Err(e) => return Err(e).context("failed to inspect session directory"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("session path must be a real directory inside the workspace");
    }
    let canonical = fs::canonicalize(&directory).context("failed to resolve session directory")?;
    if canonical.parent() != Some(workspace) {
        bail!("session directory resolves outside the workspace");
    }
    Ok(canonical)
}
fn validate_session(session: &Session, expected_id: Uuid, workspace: &Path) -> Result<()> {
    if session.version != SESSION_SCHEMA_VERSION {
        bail!(
            "unsupported session schema version {}; expected {SESSION_SCHEMA_VERSION}",
            session.version
        );
    }
    if session.session_id != expected_id {
        bail!("session ID does not match its lookup identifier");
    }
    if session.workspace != workspace {
        bail!("session workspace does not match the current workspace");
    }
    if session.model.trim().is_empty() {
        bail!("session model must not be empty");
    }
    session.context_settings.validate()?;
    let mut previous = None;
    for name in &session.mcp_tools {
        if parse_namespaced_name(name).is_none() {
            bail!("session contains an invalid MCP tool name");
        }
        if previous
            .as_ref()
            .is_some_and(|value: &&String| *value >= name)
        {
            bail!("session MCP tool names must be unique and sorted");
        }
        previous = Some(name);
    }
    if session.state.max_steps == 0 {
        bail!("session max_steps must be greater than zero");
    }
    if session.updated_at < session.created_at {
        bail!("session updated_at precedes created_at");
    }
    let expected = workspace
        .join("traces")
        .join(format!("{}.jsonl", session.session_id));
    if session.trace_path != expected {
        bail!("session trace path does not match its workspace and identifier");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{AgentState, ContextSummaryMetadata},
        test_support::TestWorkspace,
    };
    fn fixture(w: &Path, id: Uuid) -> Session {
        Session::new(
            id,
            w,
            "model",
            ContextSettings::default(),
            vec![],
            AgentState::new("task"),
            fs::canonicalize(w)
                .unwrap()
                .join("traces")
                .join(format!("{id}.jsonl")),
        )
        .unwrap()
    }
    #[test]
    fn roundtrip_preserves_summary_and_settings() {
        let w = TestWorkspace::new("session");
        fs::create_dir(w.path().join("traces")).unwrap();
        let repo = FileSessionRepository::new(w.path()).unwrap();
        let id = Uuid::from_u128(1);
        let mut session = fixture(w.path(), id);
        session.mcp_tools = vec!["mcp__local__echo".into()];
        session.state.context_summary = Some(ContextSummaryMetadata {
            summary: "summary".into(),
            omitted_messages: 2,
            omitted_groups: 1,
            estimated_tokens: 50,
            built_at_step: 2,
        });
        repo.create(&session).unwrap();
        assert_eq!(repo.load(id).unwrap(), session);
        let encoded = serde_json::to_string(&session).unwrap();
        assert!(encoded.contains("mcp__local__echo"));
        for forbidden in ["api_key", "command", "env_allowlist", "MCP_CONFIG_PATH"] {
            assert!(!encoded.contains(forbidden));
        }
    }
    #[test]
    fn clearly_rejects_older_schema() {
        let w = TestWorkspace::new("old-session");
        fs::create_dir(w.path().join("traces")).unwrap();
        let repo = FileSessionRepository::new(w.path()).unwrap();
        let id = Uuid::from_u128(2);
        let session = fixture(w.path(), id);
        repo.create(&session).unwrap();
        let mut value = serde_json::to_value(session).unwrap();
        value["version"] = serde_json::json!(1);
        fs::write(repo.session_path(id), serde_json::to_vec(&value).unwrap()).unwrap();
        let error = repo.load(id).unwrap_err();
        assert!(format!("{error:#}").contains("does not migrate older sessions"));
    }
}

#[cfg(test)]
mod hardening_regression_tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    fn fixture(workspace: &Path, id: Uuid) -> Session {
        Session::new(
            id,
            workspace,
            "model",
            ContextSettings::default(),
            vec![],
            AgentState::new("task"),
            fs::canonicalize(workspace)
                .unwrap()
                .join("traces")
                .join(format!("{id}.jsonl")),
        )
        .unwrap()
    }

    #[test]
    fn lease_prevents_concurrent_resume() {
        let workspace = TestWorkspace::new("session-lease");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let id = Uuid::from_u128(31);
        repository.create(&fixture(workspace.path(), id)).unwrap();
        let lease = repository.acquire(id).unwrap();
        assert!(repository.acquire(id).is_err());
        drop(lease);
        assert!(repository.acquire(id).is_ok());
    }

    #[test]
    fn rejects_missing_and_future_schema_versions_clearly() {
        let workspace = TestWorkspace::new("session-version");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let id = Uuid::from_u128(32);
        repository.create(&fixture(workspace.path(), id)).unwrap();
        fs::write(repository.session_path(id), b"{\"model\":\"x\"}").unwrap();
        assert!(format!("{:#}", repository.load(id).unwrap_err()).contains("no schema version"));
        let mut future = serde_json::to_value(fixture(workspace.path(), id)).unwrap();
        future["version"] = serde_json::json!(99);
        fs::write(
            repository.session_path(id),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        assert!(
            format!("{:#}", repository.load(id).unwrap_err())
                .contains("unsupported session schema version 99")
        );
    }

    #[test]
    fn rejects_invalid_persisted_context_settings() {
        let workspace = TestWorkspace::new("session-settings");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let id = Uuid::from_u128(33);
        repository.create(&fixture(workspace.path(), id)).unwrap();
        let mut value = serde_json::to_value(fixture(workspace.path(), id)).unwrap();
        value["context_settings"]["token_budget"] = serde_json::json!(0);
        fs::write(
            repository.session_path(id),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(repository.load(id).is_err());
    }
}
