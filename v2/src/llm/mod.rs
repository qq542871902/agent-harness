mod client;
mod openai;
mod types;

pub use client::LlmClient;
pub use openai::OpenAiCompatibleClient;
pub use types::{
    ChatRequest, FunctionDefinition, Message, ModelResponse, ToolCall, ToolDefinition,
    ToolDefinitionKind,
};
