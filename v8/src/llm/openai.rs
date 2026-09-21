use super::{
    ChatRequest, LlmClient, ModelResponse, ToolCall,
    types::{OpenAiMessage, OpenAiResponse},
};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use reqwest::{Client, Response, StatusCode, Url};
use std::time::Duration;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BODY_BYTES: usize = 2_097_152;
#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    http: Client,
    api_key: String,
    chat_completions_url: Url,
}
impl OpenAiCompatibleClient {
    pub fn new(config: &Config) -> Result<Self> {
        let mut base_url = Url::parse(&config.base_url)
            .with_context(|| format!("OPENAI_BASE_URL is not a valid URL: {}", config.base_url))?;
        if !matches!(base_url.scheme(), "http" | "https") {
            bail!("OPENAI_BASE_URL must use https");
        }
        let host = base_url
            .host_str()
            .context("OPENAI_BASE_URL must include a host")?;
        if base_url.scheme() == "http"
            && (!config.allow_insecure_loopback || !is_loopback_host(host))
        {
            bail!(
                "OPENAI_BASE_URL must use https; plaintext http is allowed only for an explicit loopback address when OPENAI_ALLOW_INSECURE_LOOPBACK=true"
            );
        }
        if !base_url.username().is_empty() || base_url.password().is_some() {
            bail!("OPENAI_BASE_URL must not contain credentials");
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            bail!("OPENAI_BASE_URL must not contain a query or fragment");
        }
        if base_url.cannot_be_a_base() {
            bail!("OPENAI_BASE_URL must be a hierarchical base URL");
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let chat_completions_url = base_url
            .join("chat/completions")
            .context("failed to construct chat-completions URL")?;
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self {
            http,
            api_key: config.api_key.clone(),
            chat_completions_url,
        })
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[async_trait::async_trait]
impl LlmClient for OpenAiCompatibleClient {
    async fn chat(&self, request: ChatRequest) -> Result<ModelResponse> {
        let response = self
            .http
            .post(self.chat_completions_url.clone())
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("LLM request failed")?;
        let status = response.status();
        let body = read_response_body(response).await?;
        if status != StatusCode::OK {
            bail!("LLM API returned {status}: {}", summarize_error(&body));
        }
        let response: OpenAiResponse = serde_json::from_str(&body)
            .context("LLM API returned an invalid chat-completions response")?;
        let message = response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .context("LLM API response did not include a choice")?;
        parse_model_response(message)
    }
}
async fn read_response_body(mut response: Response) -> Result<String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        bail!("LLM response exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read LLM response body")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            bail!("LLM response exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit");
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).context("LLM response body is not valid UTF-8")
}
fn parse_model_response(message: OpenAiMessage) -> Result<ModelResponse> {
    let tool_calls = message
        .tool_calls
        .into_iter()
        .map(|call| {
            if call.kind != "function" {
                bail!(
                    "unsupported tool call type `{}` for call `{}`",
                    call.kind,
                    call.id
                );
            }
            let name = call.function.name;
            let arguments = serde_json::from_str(&call.function.arguments).with_context(|| {
                format!(
                    "tool call `{}` ({name}) contained invalid JSON arguments",
                    call.id
                )
            })?;
            Ok(ToolCall {
                id: call.id,
                name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if tool_calls.is_empty()
        && message
            .content
            .as_deref()
            .is_none_or(|content| content.trim().is_empty())
    {
        bail!("LLM API response did not include assistant content or tool calls");
    }
    Ok(ModelResponse {
        content: message.content,
        tool_calls,
    })
}
fn summarize_error(body: &str) -> String {
    const MAX: usize = 500;
    let body = body.trim();
    if body.chars().count() <= MAX {
        return body.to_owned();
    }
    format!("{}…", body.chars().take(MAX).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(base_url: &str) -> Config {
        Config {
            api_key: "secret".into(),
            base_url: base_url.into(),
            allow_insecure_loopback: false,
            model: "model".into(),
            context: crate::context::ContextSettings::default(),
        }
    }
    #[test]
    fn joins_chat_path_without_discarding_base_path() {
        let client = OpenAiCompatibleClient::new(&config("https://example.test/api/v1")).unwrap();
        assert_eq!(
            client.chat_completions_url.as_str(),
            "https://example.test/api/v1/chat/completions"
        );
    }
    #[test]
    fn rejects_non_http_urls_and_embedded_credentials() {
        assert!(OpenAiCompatibleClient::new(&config("file:///tmp/provider")).is_err());
        assert!(
            OpenAiCompatibleClient::new(&config("https://user:password@example.test/v1")).is_err()
        );
    }
}
