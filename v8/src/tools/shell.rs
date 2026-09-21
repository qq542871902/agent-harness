use super::{Tool, ToolOutput, tool::canonical_workspace};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{Instant, timeout},
};
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_STREAM_BYTES: usize = 262_144;
const TRUNCATED_SUFFIX: &str = "\n[output truncated]";
const SAFE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
];

fn safe_environment() -> Vec<(&'static str, OsString)> {
    SAFE_ENV_ALLOWLIST
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (*name, value)))
        .collect()
}
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
async fn terminate_child(child: &mut Child, pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context("failed to terminate command process group");
            }
        }
    }
    #[cfg(not(unix))]
    child
        .kill()
        .await
        .context("failed to terminate timed-out command")?;
    let _ = child.wait().await;
    Ok(())
}
#[cfg(unix)]
fn terminate_process_group(pid: u32) -> Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("failed to terminate command process group");
        }
    }
    Ok(())
}
struct ChildProcess {
    child: Child,
    pid: u32,
    armed: bool,
}

impl ChildProcess {
    async fn terminate(&mut self) -> Result<()> {
        terminate_child(&mut self.child, self.pid).await?;
        self.armed = false;
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        {
            let _ = unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
        }
        let _ = self.child.start_kill();
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
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
            .envs(safe_environment());
        #[cfg(unix)]
        command.process_group(0);
        let started = Instant::now();
        let child = command
            .spawn()
            .with_context(|| format!("failed to start `{}`", arguments.command))?;
        let pid = child
            .id()
            .context("spawned command did not have a process ID")?;
        let mut process = ChildProcess {
            child,
            pid,
            armed: true,
        };
        let stdout_task = tokio::spawn(read_bounded(
            process
                .child
                .stdout
                .take()
                .context("failed to capture command stdout")?,
        ));
        let stderr_task = tokio::spawn(read_bounded(
            process
                .child
                .stderr
                .take()
                .context("failed to capture command stderr")?,
        ));
        let status: ExitStatus = match timeout(SHELL_TIMEOUT, process.child.wait()).await {
            Ok(status) => status.context("failed to wait for command")?,
            Err(_) => {
                process.terminate().await?;
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
                #[cfg(unix)]
                terminate_process_group(pid)?;
                process.disarm();
                bail!(
                    "command output timed out after {} seconds",
                    SHELL_TIMEOUT.as_secs()
                );
            }
        };
        process.disarm();
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
        let t = ShellTool::new(TestWorkspace::new("shell-strict").path()).unwrap();
        assert!(t.execute(json!({"command":"echo unsafe"})).await.is_err());
        assert!(
            t.execute(json!({"command":"cargo check --release"}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn returns_structured_output_for_nonzero_exit() {
        let w = TestWorkspace::new("shell-nonzero");
        let o = ShellTool::new(w.path())
            .unwrap()
            .execute(json!({"command":"git status"}))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&o.content).unwrap();
        assert_ne!(value["exit_code"], 0);
        assert!(o.success);
    }
}
