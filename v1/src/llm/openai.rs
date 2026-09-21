use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode};

use crate::config::Config;

use super::{
    ChatRequest, LlmClient, ModelResponse, ToolCall,
    types::{OpenAiMessage, OpenAiResponse},
};

/// Client for the OpenAI Chat Completions API and compatible services.
#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    http: Client,
    api_key: String,
    chat_completions_url: String,
}

impl OpenAiCompatibleClient {
    pub fn new(config: &Config) -> Result<Self> {
        let http = Client::builder()
            .build()
            .context("failed to create HTTP client")?;

        Ok(Self {
            http,
            api_key: config.api_key.clone(),
            chat_completions_url: format!("{}/chat/completions", config.base_url),
        })
    }
}

#[async_trait::async_trait]
impl LlmClient for OpenAiCompatibleClient {
    async fn chat(&self, request: ChatRequest) -> Result<ModelResponse> {
        let response = self
            .http
            .post(&self.chat_completions_url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("LLM request failed")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read LLM response body")?;

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

fn parse_model_response(message: OpenAiMessage) -> Result<ModelResponse> {
    if !message.tool_calls.is_empty() {
        let calls = message
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
                let arguments =
                    serde_json::from_str(&call.function.arguments).with_context(|| {
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

        return Ok(ModelResponse::ToolCalls { calls });
    }

    let content = message
        .content
        .filter(|content| !content.trim().is_empty())
        .context("LLM API response did not include assistant content")?;

    Ok(ModelResponse::Final { content })
}

fn summarize_error(body: &str) -> String {
    const MAX_ERROR_BODY_LEN: usize = 500;
    let body = body.trim();
    if body.chars().count() <= MAX_ERROR_BODY_LEN {
        return body.to_owned();
    }

    let truncated: String = body.chars().take(MAX_ERROR_BODY_LEN).collect();
    format!("{truncated}…")
}
