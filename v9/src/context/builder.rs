use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    agent::ContextSummaryMetadata,
    llm::{ChatRequest, Message, Role, ToolDefinition},
};

pub const DEFAULT_CONTEXT_TOKEN_BUDGET: usize = 16_384;
pub const DEFAULT_MAX_TOOL_OUTPUT_CHARS: usize = 32_768;
pub const DEFAULT_MAX_SUMMARY_CHARS: usize = 2_048;
const MIN_TOKEN_BUDGET: usize = 64;
const MAX_TOKEN_BUDGET: usize = 1_000_000;
const MIN_OUTPUT_CHARS: usize = 128;
const MAX_OUTPUT_CHARS: usize = 1_048_576;
const MIN_SUMMARY_CHARS: usize = 128;
const MAX_SUMMARY_CHARS: usize = 16_384;

/// Non-secret context policy persisted with a session for deterministic resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSettings {
    pub token_budget: usize,
    pub max_tool_output_chars: usize,
    pub max_summary_chars: usize,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            token_budget: DEFAULT_CONTEXT_TOKEN_BUDGET,
            max_tool_output_chars: DEFAULT_MAX_TOOL_OUTPUT_CHARS,
            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
        }
    }
}

impl ContextSettings {
    pub fn from_optional_values(
        token_budget: Option<&str>,
        max_tool_output_chars: Option<&str>,
    ) -> Result<Self> {
        let settings = Self {
            token_budget: parse_optional_usize(
                "CONTEXT_TOKEN_BUDGET",
                token_budget,
                DEFAULT_CONTEXT_TOKEN_BUDGET,
                MIN_TOKEN_BUDGET,
                MAX_TOKEN_BUDGET,
            )?,
            max_tool_output_chars: parse_optional_usize(
                "MAX_TOOL_OUTPUT_CHARS",
                max_tool_output_chars,
                DEFAULT_MAX_TOOL_OUTPUT_CHARS,
                MIN_OUTPUT_CHARS,
                MAX_OUTPUT_CHARS,
            )?,
            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
        };
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> Result<()> {
        validate_range(
            "context token budget",
            self.token_budget,
            MIN_TOKEN_BUDGET,
            MAX_TOKEN_BUDGET,
        )?;
        validate_range(
            "maximum tool output characters",
            self.max_tool_output_chars,
            MIN_OUTPUT_CHARS,
            MAX_OUTPUT_CHARS,
        )?;
        validate_range(
            "maximum summary characters",
            self.max_summary_chars,
            MIN_SUMMARY_CHARS,
            MAX_SUMMARY_CHARS,
        )
    }
}

fn parse_optional_usize(
    name: &str,
    value: Option<&str>,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize> {
    let Some(value) = value else {
        return Ok(default);
    };
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} must not be empty when set");
    }
    let parsed = value
        .parse::<usize>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    validate_range(name, parsed, minimum, maximum)?;
    Ok(parsed)
}

fn validate_range(name: &str, value: usize, minimum: usize, maximum: usize) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        bail!("{name} must be between {minimum} and {maximum}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMetadata {
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub original_messages: usize,
    pub retained_messages: usize,
    pub omitted_messages: usize,
    pub omitted_groups: usize,
    pub compacted: bool,
}

pub struct ContextBuild {
    pub request: ChatRequest,
    pub metadata: ContextMetadata,
    pub summary: Option<ContextSummaryMetadata>,
}

#[derive(Debug, Clone)]
pub struct ContextBuilder {
    settings: ContextSettings,
    sensitive_values: Vec<String>,
}

impl ContextBuilder {
    #[cfg(test)]
    pub fn new(settings: ContextSettings) -> Result<Self> {
        Self::with_sensitive_values(settings, std::iter::empty::<String>())
    }

    pub fn with_sensitive_values(
        settings: ContextSettings,
        values: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        settings.validate()?;
        let mut sensitive_values = values
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        sensitive_values.sort();
        sensitive_values.dedup();
        Ok(Self {
            settings,
            sensitive_values,
        })
    }

    pub fn settings(&self) -> &ContextSettings {
        &self.settings
    }

    pub fn build(
        &self,
        model: &str,
        canonical: &[Message],
        tools: Vec<ToolDefinition>,
        step: usize,
    ) -> Result<ContextBuild> {
        let (mandatory, groups) = coherent_groups(canonical)?;
        let mandatory_request = request(model, mandatory.clone(), tools.clone());
        let mandatory_tokens = estimate_request_tokens(&mandatory_request)?;
        if mandatory_tokens > self.settings.token_budget {
            bail!(
                "mandatory context exceeds token budget: estimated {mandatory_tokens} tokens for model, tool definitions, coding system prompt, and original user task; budget is {}",
                self.settings.token_budget
            );
        }

        let full_request = request(model, canonical.to_vec(), tools.clone());
        let full_tokens = estimate_request_tokens(&full_request)?;
        if full_tokens <= self.settings.token_budget {
            return Ok(ContextBuild {
                request: full_request,
                metadata: ContextMetadata {
                    token_budget: self.settings.token_budget,
                    estimated_tokens: full_tokens,
                    original_messages: canonical.len(),
                    retained_messages: canonical.len(),
                    omitted_messages: 0,
                    omitted_groups: 0,
                    compacted: false,
                },
                summary: None,
            });
        }

        // At least the newest coherent post-task group is mandatory whenever one exists.
        // If it cannot fit with a structural summary, fail instead of summarizing it away.
        for omitted_groups in 1..groups.len() {
            let omitted = &groups[..omitted_groups];
            let retained = &groups[omitted_groups..];
            let omitted_messages = omitted.iter().map(Vec::len).sum::<usize>();
            let summary = summarize(
                omitted,
                omitted_groups,
                omitted_messages,
                &self.sensitive_values,
            );
            let minimum_chars = summary.required_chars();
            if minimum_chars > self.settings.max_summary_chars {
                continue;
            }

            let mut low = minimum_chars;
            let mut high = self.settings.max_summary_chars;
            let mut best = None;
            while low <= high {
                let cap = low + (high - low) / 2;
                let summary_text = summary
                    .render(cap)
                    .expect("cap is at least the required structural summary size");
                let mut messages = mandatory.clone();
                messages.push(Message::system(summary_text.clone()));
                messages.extend(retained.iter().flat_map(|group| group.iter().cloned()));
                let candidate = request(model, messages, tools.clone());
                let estimated_tokens = estimate_request_tokens(&candidate)?;
                if estimated_tokens <= self.settings.token_budget {
                    best = Some((candidate, summary_text, estimated_tokens));
                    low = cap.saturating_add(1);
                } else if cap == 0 {
                    break;
                } else {
                    high = cap - 1;
                }
            }

            if let Some((candidate, summary_text, estimated_tokens)) = best {
                let retained_messages = candidate.messages.len();
                return Ok(ContextBuild {
                    request: candidate,
                    metadata: ContextMetadata {
                        token_budget: self.settings.token_budget,
                        estimated_tokens,
                        original_messages: canonical.len(),
                        retained_messages,
                        omitted_messages,
                        omitted_groups,
                        compacted: true,
                    },
                    summary: Some(ContextSummaryMetadata {
                        summary: summary_text,
                        omitted_messages,
                        omitted_groups,
                        estimated_tokens,
                        built_at_step: step,
                    }),
                });
            }
        }

        bail!(
            "mandatory context, newest coherent history group, and the required bounded history summary exceed token budget {}; increase CONTEXT_TOKEN_BUDGET",
            self.settings.token_budget
        )
    }
}

fn request(model: &str, messages: Vec<Message>, tools: Vec<ToolDefinition>) -> ChatRequest {
    ChatRequest::from_messages(model, messages).with_tools(tools)
}

/// Deterministic approximation: compact request JSON UTF-8 bytes divided by four, rounded up.
pub fn estimate_request_tokens(request: &ChatRequest) -> Result<usize> {
    let bytes = serde_json::to_vec(request)
        .context("failed to estimate request tokens")?
        .len();
    Ok(bytes.div_ceil(4))
}

fn coherent_groups(canonical: &[Message]) -> Result<(Vec<Message>, Vec<Vec<Message>>)> {
    if canonical.len() < 2 || canonical[0].role != Role::System || canonical[1].role != Role::User {
        bail!("canonical history must begin with the coding system prompt and original user task");
    }
    let mandatory = canonical[..2].to_vec();
    let mut groups = Vec::new();
    let mut index = 2;
    while index < canonical.len() {
        let message = &canonical[index];
        if message.role == Role::Tool {
            bail!("canonical history contains an orphan tool result at message {index}");
        }
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            let mut group = vec![message.clone()];
            for call in &message.tool_calls {
                index += 1;
                let Some(result) = canonical.get(index) else {
                    bail!(
                        "assistant tool call `{}` has no following tool result",
                        call.id
                    );
                };
                if result.role != Role::Tool
                    || result.tool_call_id.as_deref() != Some(call.id.as_str())
                {
                    bail!(
                        "assistant tool call `{}` is not followed by its same-id tool result",
                        call.id
                    );
                }
                group.push(result.clone());
            }
            groups.push(group);
        } else {
            groups.push(vec![message.clone()]);
        }
        index += 1;
    }
    Ok((mandatory, groups))
}

fn summarize(
    groups: &[Vec<Message>],
    omitted_groups: usize,
    omitted_messages: usize,
    sensitive_values: &[String],
) -> SummaryParts {
    use std::collections::BTreeMap;

    let mut role_counts = [0usize; 4];
    let mut call_names = BTreeMap::new();
    let mut tool_outcomes = BTreeMap::<String, (usize, usize, usize)>::new();
    let mut details = Vec::new();
    for group in groups {
        for message in group {
            role_counts[role_index(&message.role)] += 1;
            if !message.tool_calls.is_empty() {
                for call in &message.tool_calls {
                    let name = safe_tool_name(&call.function.name, sensitive_values);
                    call_names.insert(call.id.clone(), name.clone());
                    tool_outcomes.entry(name).or_default().0 += 1;
                }
            } else if message.role == Role::Tool {
                let content = message.content.as_deref().unwrap_or_default();
                let failed = is_failure(content);
                let name = message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| call_names.get(id))
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_owned());
                let outcome = tool_outcomes.entry(name.clone()).or_default();
                if failed {
                    outcome.2 += 1;
                } else {
                    outcome.1 += 1;
                }
                details.push(format!(
                    "tool:{name}:{}",
                    safe_snippet(content, sensitive_values)
                ));
            } else if let Some(content) = message.content.as_deref() {
                details.push(format!(
                    "{}:{}",
                    role_name(&message.role),
                    safe_snippet(content, sensitive_values)
                ));
            }
        }
    }

    let total_tool_names = tool_outcomes.len();
    let mut tools = tool_outcomes
        .into_iter()
        .take(16)
        .map(|(name, (calls, success, failure))| {
            format!("{name}:calls={calls},success={success},failure={failure}")
        })
        .collect::<Vec<_>>();
    if total_tool_names > tools.len() {
        tools.push(format!("+{} other_tools", total_tool_names - tools.len()));
    }
    let prefix = format!(
        "Context summary (deterministic): omitted_groups={omitted_groups}; omitted_messages={omitted_messages}; roles=system:{},user:{},assistant:{},tool:{}; tools=[{}]; snippets=[",
        role_counts[0],
        role_counts[1],
        role_counts[2],
        role_counts[3],
        tools.join(" | ")
    );
    SummaryParts {
        prefix,
        details: details.join(" | "),
    }
}

struct SummaryParts {
    prefix: String,
    details: String,
}

impl SummaryParts {
    fn required_chars(&self) -> usize {
        self.prefix.chars().count() + 1
    }

    fn render(&self, max_chars: usize) -> Option<String> {
        let required_chars = self.required_chars();
        if required_chars > max_chars {
            return None;
        }
        let detail_limit = max_chars - required_chars;
        Some(format!(
            "{}{}]",
            self.prefix,
            truncate_summary(&self.details, detail_limit)
        ))
    }
}

fn is_failure(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("failed") || lower.contains("denied") || lower.contains("rejected")
}

fn safe_tool_name(name: &str, sensitive_values: &[String]) -> String {
    let redacted = redact_known_values(name, sensitive_values);
    truncate_summary(&redacted, 32)
}

fn role_index(role: &Role) -> usize {
    match role {
        Role::System => 0,
        Role::User => 1,
        Role::Assistant => 2,
        Role::Tool => 3,
    }
}
fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn safe_snippet(content: &str, sensitive_values: &[String]) -> String {
    let redacted = content
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
                line.split_whitespace()
                    .map(|token| redact_token(token, sensitive_values))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    truncate_summary(&redacted, 80)
}

fn redact_token(token: &str, sensitive_values: &[String]) -> String {
    let redacted = redact_known_values(token, sensitive_values);
    let has_alpha = redacted.chars().any(char::is_alphabetic);
    let has_digit = redacted.chars().any(|character| character.is_ascii_digit());
    if redacted.chars().count() >= 16
        && (has_alpha && has_digit
            || redacted.starts_with("sk-")
            || redacted.starts_with("key-")
            || redacted.starts_with("token-"))
    {
        "[REDACTED]".to_owned()
    } else {
        redacted
    }
}

fn redact_known_values(value: &str, sensitive_values: &[String]) -> String {
    sensitive_values
        .iter()
        .fold(value.to_owned(), |redacted, secret| {
            redacted.replace(secret, "[REDACTED]")
        })
}

fn truncate_summary(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".into();
    }
    let mut result = value.chars().take(max_chars - 1).collect::<String>();
    result.push('…');
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedText {
    pub content: String,
    pub original_chars: usize,
    pub retained_chars: usize,
    pub truncated: bool,
}

/// Truncates by Unicode scalar values, preserving both head and tail plus an explicit size marker.
pub fn truncate_tool_output(content: &str, max_chars: usize) -> TruncatedText {
    let original_chars = content.chars().count();
    if original_chars <= max_chars {
        return TruncatedText {
            content: content.to_owned(),
            original_chars,
            retained_chars: original_chars,
            truncated: false,
        };
    }
    let marker = format!("\n…[tool output truncated; original_chars={original_chars}]…\n");
    let marker_chars = marker.chars().count();
    let available = max_chars.saturating_sub(marker_chars);
    let head_chars = available.div_ceil(2);
    let tail_chars = available / 2;
    let head = content.chars().take(head_chars).collect::<String>();
    let tail = content
        .chars()
        .skip(original_chars - tail_chars)
        .collect::<String>();
    let output = format!("{head}{marker}{tail}");
    let retained_chars = output.chars().count();
    TruncatedText {
        content: output,
        original_chars,
        retained_chars,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FunctionDefinition, ToolCall, ToolDefinitionKind};
    use serde_json::json;

    fn settings(budget: usize) -> ContextSettings {
        ContextSettings {
            token_budget: budget,
            max_tool_output_chars: 128,
            max_summary_chars: 512,
        }
    }
    fn tool() -> ToolDefinition {
        ToolDefinition {
            kind: ToolDefinitionKind::Function,
            function: FunctionDefinition {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type":"object"}),
            },
        }
    }
    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path":"x"}),
        }
    }
    fn history() -> Vec<Message> {
        let call = call("old");
        vec![
            Message::system("system"),
            Message::user("original"),
            Message::assistant("old answer ".repeat(80)),
            Message::assistant_tool_calls(None, &[call]).unwrap(),
            Message::tool_result("old", "success ".repeat(80)),
            Message::assistant("recent"),
        ]
    }

    #[test]
    fn under_budget_is_identity_and_accounts_for_tools_and_model() {
        let messages = history();
        let builder = ContextBuilder::new(settings(10_000)).unwrap();
        let built = builder.build("model", &messages, vec![tool()], 1).unwrap();
        assert_eq!(built.request.messages, messages);
        assert!(!built.metadata.compacted);
        assert_eq!(
            built.metadata.estimated_tokens,
            estimate_request_tokens(&built.request).unwrap()
        );
        let without_tools = ChatRequest::from_messages("model", built.request.messages.clone());
        assert!(built.metadata.estimated_tokens > estimate_request_tokens(&without_tools).unwrap());
    }

    #[test]
    fn removes_oldest_groups_and_keeps_tool_pairs() {
        let messages = history();
        let mandatory =
            ChatRequest::from_messages("model", messages[..2].to_vec()).with_tools(vec![tool()]);
        let budget = estimate_request_tokens(&mandatory).unwrap() + 180;
        let built = ContextBuilder::new(settings(budget))
            .unwrap()
            .build("model", &messages, vec![tool()], 2)
            .unwrap();
        assert!(built.metadata.compacted && built.metadata.omitted_groups > 0);
        for (index, message) in built.request.messages.iter().enumerate() {
            if !message.tool_calls.is_empty() {
                assert_eq!(
                    built.request.messages[index + 1].tool_call_id.as_deref(),
                    Some(message.tool_calls[0].id.as_str())
                );
            }
            assert!(
                !(message.role == Role::Tool
                    && (index == 0 || built.request.messages[index - 1].tool_calls.is_empty()))
            );
        }
        assert_eq!(
            built.request.messages.last().unwrap().content.as_deref(),
            Some("recent")
        );
    }

    #[test]
    fn mandatory_overflow_is_fatal() {
        let messages = vec![
            Message::system("s".repeat(1000)),
            Message::user("u".repeat(1000)),
        ];
        let error = ContextBuilder::new(settings(64))
            .unwrap()
            .build("model", &messages, vec![], 1)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("mandatory context exceeds"));
    }

    #[test]
    fn summary_is_deterministic_bounded_and_redacted() {
        let call = call("x");
        let messages = vec![
            Message::system("s"),
            Message::user("u"),
            Message::assistant("authorization: secret-value ".repeat(80)),
            Message::assistant_tool_calls(None, &[call]).unwrap(),
            Message::tool_result("x", "failed safely ".repeat(80)),
            Message::assistant("latest"),
        ];
        let mandatory = ChatRequest::from_messages("m", messages[..2].to_vec());
        let budget = estimate_request_tokens(&mandatory).unwrap() + 150;
        let builder = ContextBuilder::new(settings(budget)).unwrap();
        let first = builder.build("m", &messages, vec![], 3).unwrap();
        let second = builder.build("m", &messages, vec![], 3).unwrap();
        assert_eq!(first.summary, second.summary);
        let summary = first.summary.unwrap();
        assert!(summary.summary.chars().count() <= 512 && summary.summary.contains("roles="));
        assert!(!summary.summary.contains("secret-value"));
    }

    #[test]
    fn unicode_output_truncation_preserves_head_tail_and_limit() {
        let input = "αβγ😀中文尾巴".repeat(30);
        let truncated = truncate_tool_output(&input, 128);
        assert!(truncated.truncated);
        assert_eq!(truncated.original_chars, input.chars().count());
        assert_eq!(truncated.retained_chars, 128);
        assert!(truncated.content.starts_with("αβγ"));
        assert!(truncated.content.ends_with("尾巴"));
        assert!(truncated.content.contains("original_chars="));
    }

    #[test]
    fn settings_validation_rejects_malformed_and_out_of_range_values() {
        assert!(ContextSettings::from_optional_values(Some("nope"), None).is_err());
        assert!(ContextSettings::from_optional_values(Some("0"), None).is_err());
        assert!(ContextSettings::from_optional_values(None, Some("1")).is_err());
        assert_eq!(
            ContextSettings::from_optional_values(None, None).unwrap(),
            ContextSettings::default()
        );
    }
}

#[cfg(test)]
mod protocol_regression_tests {
    use super::*;
    use crate::llm::ToolCall;
    use serde_json::json;

    fn settings(budget: usize) -> ContextSettings {
        ContextSettings {
            token_budget: budget,
            max_tool_output_chars: 128,
            max_summary_chars: 512,
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": "x"}),
        }
    }

    #[test]
    fn rejects_orphan_tool_results() {
        let messages = vec![
            Message::system("system"),
            Message::user("task"),
            Message::tool_result("orphan", "result"),
        ];
        assert!(
            ContextBuilder::new(settings(1_000))
                .unwrap()
                .build("model", &messages, vec![], 1)
                .is_err()
        );
    }

    #[test]
    fn rejects_incomplete_tool_groups() {
        let pending = call("pending");
        let messages = vec![
            Message::system("system"),
            Message::user("task"),
            Message::assistant_tool_calls(None, &[pending]).unwrap(),
        ];
        let error = ContextBuilder::new(settings(1_000))
            .unwrap()
            .build("model", &messages, vec![], 1)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("no following tool result"));
    }

    #[test]
    fn multiple_tool_results_remain_with_their_call_batch() {
        let first = call("first");
        let second = call("second");
        let messages = vec![
            Message::system("system"),
            Message::user("task"),
            Message::assistant("old ".repeat(400)),
            Message::assistant_tool_calls(None, &[first, second]).unwrap(),
            Message::tool_result("first", "one"),
            Message::tool_result("second", "two"),
            Message::assistant("latest"),
        ];
        let mandatory = ChatRequest::from_messages("model", messages[..2].to_vec());
        let budget = estimate_request_tokens(&mandatory).unwrap() + 180;
        let built = ContextBuilder::new(settings(budget))
            .unwrap()
            .build("model", &messages, vec![], 1)
            .unwrap();
        let call_index = built
            .request
            .messages
            .iter()
            .position(|message| !message.tool_calls.is_empty())
            .unwrap();
        assert_eq!(
            built.request.messages[call_index + 1]
                .tool_call_id
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            built.request.messages[call_index + 2]
                .tool_call_id
                .as_deref(),
            Some("second")
        );
    }

    #[test]
    fn injected_api_key_is_never_written_to_summary() {
        let api_key = "sk-live-1234567890-sensitive";
        let messages = vec![
            Message::system("system"),
            Message::user("task"),
            Message::assistant(format!("observed {api_key} in output {}", "x ".repeat(400))),
            Message::assistant("latest"),
        ];
        let mandatory = ChatRequest::from_messages("model", messages[..2].to_vec());
        let budget = estimate_request_tokens(&mandatory).unwrap() + 120;
        let builder =
            ContextBuilder::with_sensitive_values(settings(budget), [api_key.to_owned()]).unwrap();
        let built = builder.build("model", &messages, vec![], 1).unwrap();
        let summary = built.summary.unwrap().summary;
        assert!(!summary.contains(api_key));
        assert!(
            summary.contains("roles=")
                && summary.contains("tools=[")
                && summary.contains("snippets=[")
        );
    }
}

#[cfg(test)]
mod newest_group_regression_test {
    use super::*;

    #[test]
    fn fails_instead_of_dropping_the_newest_coherent_group() {
        let messages = vec![
            Message::system("system"),
            Message::user("task"),
            Message::assistant("older history ".repeat(200)),
            Message::assistant("newest required turn ".repeat(200)),
        ];
        let mandatory = ChatRequest::from_messages("model", messages[..2].to_vec());
        let settings = ContextSettings {
            token_budget: estimate_request_tokens(&mandatory).unwrap() + 100,
            max_tool_output_chars: 128,
            max_summary_chars: 512,
        };
        let error = ContextBuilder::new(settings)
            .unwrap()
            .build("model", &messages, vec![], 1)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("required bounded history summary"));
    }
}
