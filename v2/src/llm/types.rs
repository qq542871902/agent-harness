use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A chat-completions message that can represent normal, assistant-tool-call, and tool-result turns.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub role: Role,
    /// `None` serializes as JSON null, required for assistant tool-call messages.
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<AssistantToolCall>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(content: Option<String>, calls: &[ToolCall]) -> Result<Self> {
        let tool_calls = calls
            .iter()
            .map(AssistantToolCall::from_tool_call)
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            role: Role::Assistant,
            content,
            tool_call_id: None,
            tool_calls,
        })
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// Typed function call returned by the provider; arguments are parsed once for native dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssistantToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolDefinitionKind,
    pub function: AssistantFunctionCall,
}

impl AssistantToolCall {
    fn from_tool_call(call: &ToolCall) -> Result<Self> {
        let arguments = serde_json::to_string(&call.arguments).with_context(|| {
            format!("failed to serialize arguments for tool call `{}`", call.id)
        })?;

        Ok(Self {
            id: call.id.clone(),
            kind: ToolDefinitionKind::Function,
            function: AssistantFunctionCall {
                name: call.name.clone(),
                arguments,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssistantFunctionCall {
    pub name: String,
    /// OpenAI-compatible APIs require function arguments to be a JSON-encoded string.
    pub arguments: String,
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
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

impl ChatRequest {
    pub fn from_messages(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

/// One decoded assistant model turn. Empty `tool_calls` means the turn is final.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
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
