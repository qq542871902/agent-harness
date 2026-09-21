use serde::{Deserialize, Serialize};
use serde_json::Value;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

/// An OpenAI-compatible function tool definition.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: ToolDefinitionKind,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDefinitionKind {
    Function,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

impl ChatRequest {
    pub fn from_user_prompt(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt.into(),
            }],
            tools: Vec::new(),
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelResponse {
    Final { content: String },
    ToolCalls { calls: Vec<ToolCall> },
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiResponse {
    pub choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiChoice {
    pub message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiMessage {
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<OpenAiToolCall>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: OpenAiFunctionCall,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAiFunctionCall {
    pub name: String,
    pub arguments: String,
}
