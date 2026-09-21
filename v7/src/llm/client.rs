use anyhow::Result;
use async_trait::async_trait;

use super::{ChatRequest, ModelResponse};

/// Provider-neutral interface for a single model turn.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<ModelResponse>;
}
