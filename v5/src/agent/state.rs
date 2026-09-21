use crate::llm::Message;

pub const DEFAULT_MAX_STEPS: usize = 20;

pub const CODING_AGENT_SYSTEM_PROMPT: &str = "You are a coding agent operating inside a restricted workspace. Inspect relevant files before modifying them. Make focused changes only within the workspace. After changes, run the most relevant allowed formatting, checks, and tests. Tool calls are policy checked and may require human approval. Tool failures and denied requests are observations: use them to correct your approach.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Ready,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    MaxStepsReached,
}

/// In-memory state for exactly one V5 coding-agent task.
#[derive(Debug, Clone)]
pub struct AgentState {
    pub status: AgentStatus,
    pub messages: Vec<Message>,
    /// Number of attempted model requests.
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
            messages: vec![
                Message::system(CODING_AGENT_SYSTEM_PROMPT),
                Message::user(user_prompt),
            ],
            step: 0,
            max_steps,
        }
    }
}
