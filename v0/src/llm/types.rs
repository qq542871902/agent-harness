use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

impl ChatRequest {
    pub fn from_user_prompt(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: prompt.into(),
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelResponse {
    Final { content: String },
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
}
