use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::AgentState;

pub const SESSION_SCHEMA_VERSION: u32 = 2;
const MAX_SESSION_BYTES: u64 = 4 * 1024 * 1024;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub session_id: Uuid,
    pub workspace: PathBuf,
    pub model: String,
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
    fn load(&self, session_id: Uuid) -> Result<Session>;
}

pub trait SessionSink: Send + Sync {
    fn checkpoint(&self, state: &AgentState) -> Result<()>;
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSessionSink;

#[cfg(test)]
impl SessionSink for NullSessionSink {
    fn checkpoint(&self, _state: &AgentState) -> Result<()> {
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
            // SAFETY: the descriptor remains valid for the duration of this call.
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

    /// Holds exclusive ownership of one session for the lifetime of a run/resume.
    pub fn acquire(&self, session_id: Uuid) -> Result<SessionLease> {
        let directory = checked_session_directory(&self.workspace, false)?;
        let path = directory.join(format!("{session_id}.lock"));
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
            // SAFETY: flock only observes the valid descriptor and does not retain pointers.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                bail!("session `{session_id}` is already active");
            }
        }
        #[cfg(not(unix))]
        {
            if file
                .metadata()
                .context("failed to inspect session lock")?
                .len()
                != 0
            {
                bail!("session `{session_id}` is already active");
            }
            file.set_len(1).context("failed to claim session lock")?;
        }
        Ok(SessionLease {
            #[cfg(not(unix))]
            path,
            _file: file,
        })
    }

    #[cfg(test)]
    pub fn session_path(&self, session_id: Uuid) -> PathBuf {
        self.workspace
            .join(".sessions")
            .join(format!("{session_id}.json"))
    }

    fn checked_path(&self, session_id: Uuid, must_exist: bool) -> Result<PathBuf> {
        let directory = checked_session_directory(&self.workspace, false)?;
        let path = directory.join(format!("{session_id}.json"));
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !must_exist => Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("session `{session_id}` does not exist")
            }
            Err(error) => Err(error).context("failed to inspect session file"),
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
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("failed to create session temp file"),
            }
        }
        bail!("failed to allocate a temporary session file")
    }

    fn sync_directory(&self) -> Result<()> {
        File::open(self.workspace.join(".sessions"))
            .and_then(|directory| directory.sync_all())
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
        let (temp_path, file) = self.write_temp(&bytes)?;
        drop(file);
        let result = fs::hard_link(&temp_path, &target)
            .context("failed to create new session without replacing an existing session")
            .and_then(|()| self.sync_directory());
        let _ = fs::remove_file(&temp_path);
        result
    }

    fn save(&self, session: &Session) -> Result<()> {
        let target = self.checked_path(session.session_id, true)?;
        let bytes = self.encoded(session)?;
        let (temp_path, file) = self.write_temp(&bytes)?;
        drop(file);
        let result = fs::rename(&temp_path, &target)
            .context("failed to atomically replace session")
            .and_then(|()| self.sync_directory());
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    fn load(&self, session_id: Uuid) -> Result<Session> {
        let path = self.checked_path(session_id, true)?;
        let mut file = File::open(&path).context("failed to open session")?;
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
        let session: Session =
            serde_json::from_slice(&bytes).context("session file contains invalid JSON")?;
        validate_session(&session, session_id, &self.workspace)?;
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
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            fs::create_dir(&directory).context("failed to create session directory")?;
            fs::symlink_metadata(&directory).context("failed to inspect session directory")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("session directory does not exist")
        }
        Err(error) => return Err(error).context("failed to inspect session directory"),
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
    if session.state.max_steps == 0 {
        bail!("session max_steps must be greater than zero");
    }
    if let Some(in_flight) = &session.state.in_flight_tool_call
        && !session
            .state
            .pending_tool_calls
            .iter()
            .any(|pending| pending == in_flight)
    {
        bail!("in-flight tool call must also be present in pending_tool_calls");
    }
    if session.updated_at < session.created_at {
        bail!("session updated_at precedes created_at");
    }
    let expected_trace = workspace
        .join("traces")
        .join(format!("{}.jsonl", session.session_id));
    if session.trace_path != expected_trace {
        bail!("session trace path does not match its workspace and identifier");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{AgentState, AgentStatus},
        test_support::TestWorkspace,
    };

    fn fixture(workspace: &Path, id: Uuid) -> Session {
        Session::new(
            id,
            workspace,
            "test-model",
            AgentState::new("inspect"),
            fs::canonicalize(workspace)
                .unwrap()
                .join("traces")
                .join(format!("{id}.jsonl")),
        )
        .unwrap()
    }

    #[test]
    fn round_trip_and_atomic_replacement() {
        let workspace = TestWorkspace::new("session-roundtrip");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let id = Uuid::from_u128(21);
        let mut session = fixture(workspace.path(), id);
        repository.create(&session).unwrap();
        let lease = repository.acquire(id).unwrap();
        assert!(repository.acquire(id).is_err());
        drop(lease);
        assert!(repository.acquire(id).is_ok());
        session.state.status = AgentStatus::Running;
        session.state.step = 3;
        repository.save(&session).unwrap();
        assert_eq!(repository.load(id).unwrap(), session);
        assert!(repository.create(&session).is_err());
        assert!(
            fs::read_dir(workspace.path().join(".sessions"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
    }

    #[test]
    fn rejects_corruption_oversize_version_and_workspace_mismatch() {
        let workspace = TestWorkspace::new("session-invalid");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let id = Uuid::from_u128(22);
        let mut session = fixture(workspace.path(), id);
        repository.create(&session).unwrap();
        fs::write(repository.session_path(id), b"not json").unwrap();
        assert!(repository.load(id).is_err());
        fs::write(
            repository.session_path(id),
            vec![b'x'; MAX_SESSION_BYTES as usize + 1],
        )
        .unwrap();
        assert!(repository.load(id).is_err());
        session.version += 1;
        fs::write(
            repository.session_path(id),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        assert!(repository.load(id).is_err());
        session.version = SESSION_SCHEMA_VERSION;
        session.session_id = Uuid::from_u128(999);
        fs::write(
            repository.session_path(id),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        assert!(repository.load(id).is_err());
        session.session_id = id;
        session.workspace = workspace.path().join("other");
        fs::write(
            repository.session_path(id),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        assert!(repository.load(id).is_err());
    }

    #[test]
    fn serialized_session_has_no_provider_secret_field() {
        let workspace = TestWorkspace::new("session-no-secret");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let session = fixture(workspace.path(), Uuid::from_u128(24));
        let serialized = serde_json::to_string(&session).unwrap();
        let provider_secret = "provider-key-that-must-not-be-persisted";
        assert!(!serialized.contains(provider_secret));
        assert!(!serialized.to_ascii_lowercase().contains("api_key"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_directory_and_file() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("session-symlink-directory");
        let outside = TestWorkspace::new("session-symlink-outside");
        symlink(outside.path(), workspace.path().join(".sessions")).unwrap();
        assert!(FileSessionRepository::new(workspace.path()).is_err());

        let workspace = TestWorkspace::new("session-symlink-file");
        fs::create_dir(workspace.path().join("traces")).unwrap();
        let repository = FileSessionRepository::new(workspace.path()).unwrap();
        let id = Uuid::from_u128(23);
        let outside_file = outside.path().join("outside.json");
        fs::write(&outside_file, "{}").unwrap();
        symlink(&outside_file, repository.session_path(id)).unwrap();
        assert!(repository.load(id).is_err());
        assert!(repository.create(&fixture(workspace.path(), id)).is_err());
    }
}
