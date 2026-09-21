use std::{path::Path, process::Stdio, time::Duration};

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

/// Runs one exact, allowlisted development command without invoking a shell.
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

/// Environment variables restored (when present) for the allowlisted toolchain.
/// The child environment is otherwise cleared so provider credentials such as
/// `OPENAI_API_KEY` are never inherited by workspace build scripts or tests.
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "SHELL",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "SSH_AUTH_SOCK",
];

fn apply_minimal_environment(command: &mut Command) {
    command.env_clear();
    for name in INHERITED_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
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

async fn read_bounded(mut stream: impl AsyncRead + Unpin) -> Result<String> {
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 8192];
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
        collected.extend_from_slice(TRUNCATED_SUFFIX.as_bytes());
    }
    Ok(String::from_utf8_lossy(&collected).into_owned())
}

async fn terminate_child(child: &mut Child, process_id: u32) -> Result<()> {
    #[cfg(unix)]
    {
        // The command is placed in a process group whose ID equals its process ID.
        let result = unsafe { libc::kill(-(process_id as i32), libc::SIGKILL) };
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
fn terminate_process_group(process_id: u32) -> Result<()> {
    // The group was created immediately before spawning the allowlisted command.
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
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["cargo check", "cargo test", "cargo fmt", "cargo clippy", "git diff", "git status"]
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
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
            .kill_on_drop(true);
        apply_minimal_environment(&mut command);
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start `{}`", arguments.command))?;
        let process_id = child
            .id()
            .context("spawned command did not have a process ID")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture command stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture command stderr")?;
        let stdout_task = tokio::spawn(read_bounded(stdout));
        let stderr_task = tokio::spawn(read_bounded(stderr));

        let status = match timeout(SHELL_TIMEOUT, child.wait()).await {
            Ok(status) => status.context("failed to wait for command")?,
            Err(_) => {
                terminate_child(&mut child, process_id).await?;
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
                terminate_process_group(process_id)?;
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

        Ok(ToolOutput {
            content,
            success: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_support::TestWorkspace;

    #[tokio::test]
    async fn enforces_exact_allowlist_and_strict_arguments() {
        let workspace = TestWorkspace::new("shell-strict");
        let tool = ShellTool::new(workspace.path()).unwrap();

        assert!(
            tool.execute(json!({"command": "echo unsafe"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"command": "cargo check --release"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"command": "git status", "extra": true}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn returns_structured_output_for_nonzero_exit() {
        let workspace = TestWorkspace::new("shell-nonzero");
        let tool = ShellTool::new(workspace.path()).unwrap();

        let output = tool
            .execute(json!({"command": "git status"}))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        assert_ne!(value["exit_code"], 0);
        assert!(value["stdout"].is_string());
        assert!(value["stderr"].is_string());
        assert!(output.success);
    }

    #[test]
    fn minimal_environment_excludes_provider_secret() {
        assert!(!INHERITED_ENV.contains(&"OPENAI_API_KEY"));
        // SAFETY: single-threaded test process mutation of a scoped variable.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "must-not-leak");
        }
        let mut command = Command::new("cargo");
        apply_minimal_environment(&mut command);
        let leaked = command
            .as_std()
            .get_envs()
            .any(|(name, _)| name == "OPENAI_API_KEY");
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        assert!(!leaked, "provider secret must not reach the shell child");
    }
}
