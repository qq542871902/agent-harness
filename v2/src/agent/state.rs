use crate::llm::Message;

pub const DEFAULT_MAX_STEPS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Ready,
    Running,
    Completed,
    Failed,
    MaxStepsReached,
}

/// In-memory state for exactly one V2 agent task.
#[derive(Debug, Clone)]
pub struct AgentState {
    pub status: AgentStatus,
    pub messages: Vec<Message>,
    /// Number of completed model requests.
    pub step: usize,
    pub max_steps: usize,
}

impl AgentState {
    pub fn new(user_prompt: impl Into<String>) -> Self {
        Self::with_max_steps(user_prompt, DEFAULT_MAX_STEPS)
    }

    pub fn with_max_steps(user_prompt: impl Into<String>, max_steps: usize) -> Self {
        Self {
            status: AgentStatus::Ready,
            messages: vec![Message::user(user_prompt)],
            step: 0,
            max_steps,
        }
    }
}
