mod client;
mod openai;
mod types;

pub use client::LlmClient;
pub use openai::OpenAiCompatibleClient;
pub use types::{
    ChatRequest, FunctionDefinition, ModelResponse, ToolCall, ToolDefinition, ToolDefinitionKind,
};
