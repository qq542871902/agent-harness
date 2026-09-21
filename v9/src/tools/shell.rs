use super::{Tool, ToolOutput, tool::canonical_workspace};
use crate::cancel::CancellationToken;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::{Instant, timeout},
};
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_STREAM_BYTES: usize = 262_144;
const TRUNCATED_SUFFIX: &str = "\n[output truncated]";
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
    cancellation: CancellationToken,
}
impl ShellTool {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            workspace: canonical_workspace(workspace)?,
            cancellation: CancellationToken::new(),
        })
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
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
fn configure_environment(command: &mut Command) {
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("CARGO_TERM_COLOR", "never");
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

#[cfg(unix)]
struct ProcessGroupGuard {
    pid: u32,
    armed: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = terminate_process_group(self.pid);
        }
    }
}
#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run exactly one allowlisted development command in the workspace: cargo check, cargo test, cargo fmt, cargo clippy, git diff, or git status. Cargo commands can execute workspace build scripts, procedural macros, compiler wrappers configured in project files, and test binaries; approval policy must treat them as code execution."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string","enum":["cargo check","cargo test","cargo fmt","cargo clippy","git diff","git status"]}},"required":["command"],"additionalProperties":false})
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let arguments: ShellArguments =
            serde_json::from_value(arguments).context("invalid arguments for shell")?;
        let (executable, args) = allowed_command(&arguments.command)?;
        let mut command = Command::new(executable);
        configure_environment(&mut command);
        command
            .args(args)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start `{}`", arguments.command))?;
        let pid = child
            .id()
            .context("spawned command did not have a process ID")?;
        #[cfg(unix)]
        let mut process_guard = ProcessGroupGuard::new(pid);
        let stdout_task = tokio::spawn(read_bounded(
            child
                .stdout
                .take()
                .context("failed to capture command stdout")?,
        ));
        let stderr_task = tokio::spawn(read_bounded(
            child
                .stderr
                .take()
                .context("failed to capture command stderr")?,
        ));
        let status = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => {
                terminate_child(&mut child, pid).await?;
                stdout_task.abort();
                stderr_task.abort();
                bail!("command cancelled");
            }
            status = timeout(SHELL_TIMEOUT, child.wait()) => match status {
                Ok(status) => status.context("failed to wait for command")?,
                Err(_) => {
                    terminate_child(&mut child, pid).await?;
                    stdout_task.abort();
                    stderr_task.abort();
                    bail!(
                        "command timed out after {} seconds",
                        SHELL_TIMEOUT.as_secs()
                    );
                }
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
                bail!(
                    "command output timed out after {} seconds",
                    SHELL_TIMEOUT.as_secs()
                );
            }
        };
        let content = serde_json::to_string_pretty(&ShellResult {
            exit_code: status.code(),
            stdout,
            stderr,
        })
        .context("failed to serialize shell result")?;
        #[cfg(unix)]
        process_guard.disarm();
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

#[cfg(test)]
mod environment_tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn command_environment_is_cleared_and_secret_variables_are_not_forwarded() {
        let mut command = Command::new("cargo");
        configure_environment(&mut command);
        let names = command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| value.map(|_| name.to_string_lossy().into_owned()))
            .collect::<BTreeSet<_>>();
        assert!(!names.contains("OPENAI_API_KEY"));
        assert!(!names.contains("MCP_CONFIG_PATH"));
        assert!(!names.contains("RUSTC_WRAPPER"));
        assert!(names.contains("CARGO_TERM_COLOR"));
        assert!(names.iter().all(|name| matches!(
            name.as_str(),
            "PATH"
                | "HOME"
                | "USER"
                | "LOGNAME"
                | "TMPDIR"
                | "TEMP"
                | "TMP"
                | "CARGO_HOME"
                | "RUSTUP_HOME"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "CARGO_TERM_COLOR"
        )));
    }

    #[test]
    fn description_warns_that_cargo_is_code_execution() {
        let workspace = crate::test_support::TestWorkspace::new("shell-warning");
        let tool = ShellTool::new(workspace.path()).unwrap();
        assert!(tool.description().contains("build scripts"));
        assert!(tool.description().contains("procedural macros"));
    }
}
