#[cfg(test)]
use std::io::BufRead;
use std::{
    collections::HashSet,
    io::{self, Write},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio::{io::AsyncBufReadExt, time::timeout};

use crate::llm::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    GrantOnce,
    Deny,
    AlwaysAllow,
}

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn request_approval(&self, call: &ToolCall) -> Result<ApprovalDecision>;
}

#[derive(Debug, Default)]
pub struct ApprovalState {
    always_allowed_tools: HashSet<String>,
}
impl ApprovalState {
    pub fn always_allow(&mut self, tool: impl Into<String>) {
        self.always_allowed_tools.insert(tool.into());
    }
    pub fn is_always_allowed(&self, tool: &str) -> bool {
        self.always_allowed_tools.contains(tool)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ConsoleApprovalHandler;
#[async_trait]
impl ApprovalHandler for ConsoleApprovalHandler {
    async fn request_approval(&self, call: &ToolCall) -> Result<ApprovalDecision> {
        let arguments = serde_json::to_string_pretty(&redact_sensitive_values(&call.arguments))
            .context("failed to format tool arguments for approval")?;
        {
            let mut output = io::stdout().lock();
            writeln!(output, "\nAgent wants to execute:\n\nTool:\n{}\n\nArguments:\n{}\n\nApprove?\n[y] yes\n[n] no\n[a] always allow this tool", call.name, arguments)
                .context("failed to display approval request")?;
            output.flush().context("failed to flush approval prompt")?;
        }
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        loop {
            {
                let mut output = io::stdout().lock();
                write!(output, "> ").context("failed to display approval prompt")?;
                output.flush().context("failed to flush approval prompt")?;
            }
            let mut input = String::new();
            let bytes = timeout(APPROVAL_TIMEOUT, reader.read_line(&mut input))
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "approval timed out after {} seconds",
                        APPROVAL_TIMEOUT.as_secs()
                    )
                })?
                .context("failed to read approval response")?;
            if bytes == 0 {
                bail!("no approval input was available");
            }
            match input.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(ApprovalDecision::GrantOnce),
                "n" | "no" => return Ok(ApprovalDecision::Deny),
                "a" | "always" => return Ok(ApprovalDecision::AlwaysAllow),
                _ => {
                    let mut output = io::stdout().lock();
                    writeln!(output, "Please enter y, n, or a.")
                        .context("failed to display approval guidance")?;
                }
            }
        }
    }
}

#[cfg(test)]
fn request_approval_from(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    call: &ToolCall,
) -> Result<ApprovalDecision> {
    let arguments = serde_json::to_string_pretty(&redact_sensitive_values(&call.arguments))
        .context("failed to format tool arguments for approval")?;
    writeln!(writer, "\nAgent wants to execute:\n\nTool:\n{}\n\nArguments:\n{}\n\nApprove?\n[y] yes\n[n] no\n[a] always allow this tool", call.name, arguments).context("failed to display approval request")?;
    loop {
        write!(writer, "> ").context("failed to display approval prompt")?;
        writer.flush().context("failed to flush approval prompt")?;
        let mut input = String::new();
        if reader
            .read_line(&mut input)
            .context("failed to read approval response")?
            == 0
        {
            writeln!(writer, "No input available; denying tool call.")
                .context("failed to display approval denial")?;
            return Ok(ApprovalDecision::Deny);
        }
        match input.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(ApprovalDecision::GrantOnce),
            "n" | "no" => return Ok(ApprovalDecision::Deny),
            "a" | "always" => return Ok(ApprovalDecision::AlwaysAllow),
            _ => writeln!(writer, "Please enter y, n, or a.")
                .context("failed to display approval guidance")?,
        }
    }
}

fn redact_sensitive_values(value: &Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let normalized = key.to_ascii_lowercase();
                    let value = if matches!(
                        normalized.as_str(),
                        "api_key"
                            | "apikey"
                            | "authorization"
                            | "password"
                            | "secret"
                            | "token"
                            | "access_token"
                            | "refresh_token"
                    ) {
                        Value::String("[REDACTED]".into())
                    } else {
                        redact_sensitive_values(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_sensitive_values).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    fn call() -> ToolCall {
        ToolCall {
            id: "approval".into(),
            name: "write_file".into(),
            arguments: json!({"path":"src/main.rs","token":"do-not-print","nested":{"password":"also-secret"}}),
        }
    }
    #[test]
    fn loops_until_grant_deny_or_always_and_formats_redacted_json() {
        for (input, expected) in [
            ("invalid\ny\n", ApprovalDecision::GrantOnce),
            ("n\n", ApprovalDecision::Deny),
            ("a\n", ApprovalDecision::AlwaysAllow),
        ] {
            let mut reader = Cursor::new(input.as_bytes());
            let mut output = Vec::new();
            assert_eq!(
                request_approval_from(&mut reader, &mut output, &call()).unwrap(),
                expected
            );
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("write_file") && output.contains("[REDACTED]"));
            assert!(!output.contains("do-not-print") && !output.contains("also-secret"));
        }
    }
    #[test]
    fn eof_is_a_denial() {
        assert_eq!(
            request_approval_from(&mut Cursor::new(Vec::<u8>::new()), &mut Vec::new(), &call())
                .unwrap(),
            ApprovalDecision::Deny
        );
    }
    #[test]
    fn always_allow_state_is_explicit_and_task_local() {
        let mut state = ApprovalState::default();
        assert!(!state.is_always_allowed("write_file"));
        state.always_allow("write_file");
        assert!(state.is_always_allowed("write_file"));
    }
}
