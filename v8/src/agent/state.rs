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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSummaryMetadata {
    pub summary: String,
    pub omitted_messages: usize,
    pub omitted_groups: usize,
    pub estimated_tokens: usize,
    pub built_at_step: usize,
}

/// Persisted canonical state for exactly one V7 coding-agent task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    pub status: AgentStatus,
    pub messages: Vec<Message>,
    pub step: usize,
    pub max_steps: usize,
    #[serde(default)]
    pub pending_tool_calls: Vec<ToolCall>,
    /// A durable pre-execution marker. Recovery never automatically replays this call because
    /// its external side effects may have completed before the previous process stopped.
    #[serde(default)]
    pub in_flight_tool_call: Option<ToolCall>,
    #[serde(default)]
    pub context_summary: Option<ContextSummaryMetadata>,
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
            context_summary: None,
        }
    }
    pub fn prepare_for_resume(&mut self) {
        if matches!(
            self.status,
            AgentStatus::Failed | AgentStatus::Aborted | AgentStatus::MaxStepsReached
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_starts_with_mandatory_context() {
        let state = AgentState::new("original task");
        assert_eq!(
            state.messages[0].content.as_deref(),
            Some(CODING_AGENT_SYSTEM_PROMPT)
        );
        assert_eq!(state.messages[1].content.as_deref(), Some("original task"));
        assert!(state.context_summary.is_none());
    }

    #[test]
    fn resume_preserves_completed_state_and_extends_failed_budget() {
        let mut completed = AgentState::new("task");
        completed.status = AgentStatus::Completed;
        completed.prepare_for_resume();
        assert_eq!(completed.status, AgentStatus::Completed);

        let mut failed = AgentState::with_max_steps("task", 2);
        failed.status = AgentStatus::Failed;
        failed.step = 2;
        failed.prepare_for_resume();
        assert_eq!(failed.status, AgentStatus::Ready);
        assert_eq!(failed.max_steps, 22);
    }
}
