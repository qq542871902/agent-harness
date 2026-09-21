use serde::{Deserialize, Serialize};

use crate::llm::{Message, ToolCall};

pub const DEFAULT_MAX_STEPS: usize = 20;

pub const CODING_AGENT_SYSTEM_PROMPT: &str = "You are a coding agent operating inside a restricted workspace. Inspect relevant files before modifying them. Make focused changes only within the workspace. After changes, run the most relevant allowed formatting, checks, and tests. Tool calls are policy checked and may require human approval. Tool failures and denied requests are observations: use them to correct your approach.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Ready,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Aborted,
    MaxStepsReached,
}

/// Persisted state for exactly one V6 coding-agent task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub status: AgentStatus,
    pub messages: Vec<Message>,
    /// Number of attempted model requests.
    pub step: usize,
    pub max_steps: usize,
    /// Tool calls from a persisted model turn that still need observations.
    #[serde(default)]
    pub pending_tool_calls: Vec<ToolCall>,
    /// A call whose dispatch was durably announced but whose outcome is not yet durable.
    #[serde(default)]
    pub in_flight_tool_call: Option<ToolCall>,
}

impl AgentState {
    pub fn new(user_prompt: impl Into<String>) -> Self {
        Self::with_max_steps(user_prompt, DEFAULT_MAX_STEPS)
    }

    pub fn with_max_steps(user_prompt: impl Into<String>, max_steps: usize) -> Self {
        Self {
            status: AgentStatus::Ready,
            messages: vec![
                Message::system(CODING_AGENT_SYSTEM_PROMPT),
                Message::user(user_prompt),
            ],
            step: 0,
            max_steps,
            pending_tool_calls: Vec::new(),
            in_flight_tool_call: None,
        }
    }

    pub fn prepare_for_resume(&mut self) {
        if matches!(
            self.status,
            AgentStatus::Failed | AgentStatus::MaxStepsReached
        ) || self.step >= self.max_steps
        {
            self.max_steps = self.step.saturating_add(DEFAULT_MAX_STEPS);
        }
        if self.status != AgentStatus::Completed {
            self.status = AgentStatus::Ready;
        }
    }

    pub fn final_content(&self) -> Option<&str> {
        (self.status == AgentStatus::Completed)
            .then(|| self.messages.last()?.content.as_deref())
            .flatten()
    }
}
