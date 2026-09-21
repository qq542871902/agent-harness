use anyhow::Result;
use async_trait::async_trait;

use super::{ChatRequest, ModelResponse};

/// Provider-neutral interface for sending a chat request to a language model.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ModelResponse>;
}
