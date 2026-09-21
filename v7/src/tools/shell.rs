use std::{ffi::OsString, path::Path, process::Stdio, thread, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{Instant, timeout},
};

use super::{Tool, ToolOutput, tool::canonical_workspace};

const SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_STREAM_BYTES: usize = 262_144;
const TRUNCATED_SUFFIX: &str = "\n[output truncated]";
const ALLOWED_ENVIRONMENT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUST_BACKTRACE",
    "CARGO_TERM_COLOR",
    "NO_COLOR",
    "TERM",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArguments {
    command: String,
}
#[derive(Serialize)]
struct ShellResult {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}
pub struct ShellTool {
    workspace: std::path::PathBuf,
}
impl ShellTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            workspace: canonical_workspace(workspace)?,
        })
    }
}

struct ActiveProcess {
    child: Child,
    process_id: u32,
    child_reaped: bool,
    cleanup_armed: bool,
}
impl ActiveProcess {
    fn new(child: Child, process_id: u32) -> Self {
        Self {
            child,
            process_id,
            child_reaped: false,
            cleanup_armed: true,
        }
    }
    async fn kill_and_reap(&mut self) -> Result<()> {
        terminate_process_group(&mut self.child, self.process_id).await?;
        self.child_reaped = true;
        self.cleanup_armed = false;
        Ok(())
    }
    fn disarm(&mut self) {
        self.child_reaped = true;
        self.cleanup_armed = false;
    }
}
impl Drop for ActiveProcess {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        kill_process_group_now(&mut self.child, self.process_id);
        if !self.child_reaped {
            for _ in 0..100 {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                }
            }
        }
    }
}

fn allowed_command(command: &str) -> Result<(&'static str, &'static [&'static str])> {
    match command {
        "cargo check" => Ok(("cargo", &["check"])),
        "cargo test" => Ok(("cargo", &["test"])),
        "cargo fmt" => Ok(("cargo", &["fmt"])),
        "cargo clippy" => Ok(("cargo", &["clippy"])),
        "git diff" => Ok(("git", &["diff"])),
        "git status" => Ok(("git", &["status"])),
        _ => bail!(
            "command is not allowed; expected exactly one of: cargo check, cargo test, cargo fmt, cargo clippy, git diff, git status"
        ),
    }
}
fn minimal_environment_from(
    values: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    values
        .into_iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|name| ALLOWED_ENVIRONMENT.contains(&name))
        })
        .collect()
}
async fn read_bounded(mut stream: impl AsyncRead + Unpin) -> Result<String> {
    let mut collected = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .context("failed to read command output")?;
        if read == 0 {
            break;
        }
        let remaining = MAX_STREAM_BYTES.saturating_sub(collected.len());
        if read <= remaining {
            collected.extend_from_slice(&buffer[..read]);
        } else {
            collected.extend_from_slice(&buffer[..remaining]);
            truncated = true;
        }
    }
    if truncated {
        let keep = MAX_STREAM_BYTES.saturating_sub(TRUNCATED_SUFFIX.len());
        collected.truncate(keep);
        while std::str::from_utf8(&collected).is_err() && !collected.is_empty() {
            collected.pop();
        }
        collected.extend_from_slice(TRUNCATED_SUFFIX.as_bytes());
    }
    Ok(String::from_utf8_lossy(&collected).into_owned())
}
async fn terminate_process_group(child: &mut Child, process_id: u32) -> Result<()> {
    #[cfg(unix)]
    kill_unix_process_group(process_id)?;
    #[cfg(not(unix))]
    child.kill().await.context("failed to terminate command")?;
    let _ = child.wait().await;
    Ok(())
}
#[cfg(unix)]
fn kill_process_group_now(_child: &mut Child, process_id: u32) {
    let _ = kill_unix_process_group(process_id);
}
#[cfg(not(unix))]
fn kill_process_group_now(child: &mut Child, _process_id: u32) {
    let _ = child.start_kill();
}
#[cfg(unix)]
fn kill_unix_process_group(process_id: u32) -> Result<()> {
    // SAFETY: a negative PID targets only the process group created for this child.
    let result = unsafe { libc::kill(-(process_id as i32), libc::SIGKILL) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("failed to terminate command process group");
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }
    fn description(&self) -> &'static str {
        "Run exactly one allowlisted development command in the workspace: cargo check, cargo test, cargo fmt, cargo clippy, git diff, or git status."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string","enum":["cargo check","cargo test","cargo fmt","cargo clippy","git diff","git status"]}},"required":["command"],"additionalProperties":false})
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: ShellArguments =
            serde_json::from_value(arguments).context("invalid arguments for shell")?;
        let (executable, args) = allowed_command(&arguments.command)?;
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear()
            .envs(minimal_environment_from(std::env::vars_os()));
        #[cfg(unix)]
        command.process_group(0);
        let started = Instant::now();
        let child = command
            .spawn()
            .with_context(|| format!("failed to start `{}`", arguments.command))?;
        let process_id = child
            .id()
            .context("spawned command did not have a process ID")?;
        let mut active = ActiveProcess::new(child, process_id);
        let stdout_task = tokio::spawn(read_bounded(
            active
                .child
                .stdout
                .take()
                .context("failed to capture command stdout")?,
        ));
        let stderr_task = tokio::spawn(read_bounded(
            active
                .child
                .stderr
                .take()
                .context("failed to capture command stderr")?,
        ));
        let status = match timeout(SHELL_TIMEOUT, active.child.wait()).await {
            Ok(status) => {
                active.child_reaped = true;
                status.context("failed to wait for command")?
            }
            Err(_) => {
                active.kill_and_reap().await?;
                bail!(
                    "command timed out after {} seconds",
                    SHELL_TIMEOUT.as_secs()
                );
            }
        };
        let remaining = SHELL_TIMEOUT
            .checked_sub(started.elapsed())
            .context("command output timed out")?;
        let streams = timeout(remaining, async {
            let stdout = stdout_task.await.context("stdout reader task failed")??;
            let stderr = stderr_task.await.context("stderr reader task failed")??;
            Ok::<_, anyhow::Error>((stdout, stderr))
        })
        .await;
        let (stdout, stderr) = match streams {
            Ok(streams) => streams?,
            Err(_) => {
                active.kill_and_reap().await?;
                bail!(
                    "command output timed out after {} seconds",
                    SHELL_TIMEOUT.as_secs()
                );
            }
        };
        active.disarm();
        let content = serde_json::to_string_pretty(&ShellResult {
            exit_code: status.code(),
            stdout,
            stderr,
        })
        .context("failed to serialize shell result")?;
        Ok(ToolOutput {
            content,
            success: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    #[tokio::test]
    async fn enforces_exact_allowlist_and_strict_arguments() {
        let tool = ShellTool::new(TestWorkspace::new("shell-strict").path()).unwrap();
        assert!(
            tool.execute(json!({"command":"echo unsafe"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"command":"cargo check --release"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"command":"git status","extra":true}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn returns_structured_output_for_nonzero_exit() {
        let workspace = TestWorkspace::new("shell-nonzero");
        let output = ShellTool::new(workspace.path())
            .unwrap()
            .execute(json!({"command":"git status"}))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_ne!(value["exit_code"], 0);
        assert!(output.success);
    }
    #[test]
    fn minimal_environment_excludes_secrets() {
        let environment = minimal_environment_from([
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("HOME"), OsString::from("/tmp/home")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("secret"),
            ),
        ]);
        assert_eq!(environment.len(), 2);
        assert!(environment.iter().all(|(name, _)| name != "OPENAI_API_KEY"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_active_process_kills_and_reaps_group() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().unwrap();
        let process_id = child.id().unwrap();
        drop(ActiveProcess::new(child, process_id));
        // SAFETY: signal 0 only checks whether the dedicated process group still exists.
        let result = unsafe { libc::kill(-(process_id as i32), 0) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
