//! # Agent Layer
//!
//! Defines the core abstractions shared by every agent in the HarnessCode pipeline,
//! and re-exports the four concrete agent implementations.
//!
//! ## Modules
//!
//! | Module     | Role |
//! |------------|------|
//! | [`planner`] | Decomposes the user's goal into an ordered execution plan |
//! | [`coder`]   | Implements the plan using file tools; produces a diff summary |
//! | [`risk`]    | Assesses the diff for semantic risk and security implications |
//! | [`reviewer`]| Validates correctness and decides pass/fail |

pub mod coder;
pub mod drift_judge;
pub mod planner;
pub mod reviewer;
pub mod risk;

pub use coder::LlmCoderAgent;
pub use drift_judge::{
    DriftCallback, DriftConfig, DriftDecision, DriftJudgeAgent, DriftKind, DriftParams,
    DriftSignal, TurnSummary,
};
pub use planner::LlmPlannerAgent;
pub use reviewer::LlmReviewerAgent;
pub use risk::LlmRiskAgent;

use crate::llm::LlmProvider;
use crate::observability::TokenUsage;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

// ──────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────

/// Errors that can occur during agent execution or controller orchestration.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent '{role}' failed: {message}")]
    ExecutionFailed { role: AgentRole, message: String },

    #[error("maximum retry limit ({0}) reached without converging")]
    MaxRetriesExceeded(usize),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("LLM provider error: {0}")]
    Provider(#[from] crate::llm::LlmError),

    /// A safety guardrail was triggered during execution.
    #[error("guardrail violation: {0}")]
    GuardrailViolation(#[from] crate::controller::guardrails::GuardrailViolation),

    /// The user decided to abort the pipeline after drift was detected.
    #[error("pipeline aborted by user after drift detection")]
    DriftAborted,

    /// The user requested a pipeline restart with a reinforced prompt.
    #[error("drift restart requested")]
    DriftRestart { reinforced_prompt: String },
}

// ──────────────────────────────────────────────
// Agent role
// ──────────────────────────────────────────────

/// Identifies which role in the pipeline an agent fulfils.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    /// Breaks the user's goal into discrete, verifiable steps.
    Planner,
    /// Writes and applies code changes inside the sandbox.
    Coder,
    /// Analyses the code diff for semantic risk and tags it for CR awareness.
    Risk,
    /// Validates the output, runs tests, and decides pass/fail.
    Reviewer,
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentRole::Planner => write!(f, "Planner"),
            AgentRole::Coder => write!(f, "Coder"),
            AgentRole::Risk => write!(f, "Risk"),
            AgentRole::Reviewer => write!(f, "Reviewer"),
        }
    }
}

// ──────────────────────────────────────────────
// Agent output
// ──────────────────────────────────────────────

/// The structured output produced by an [`Agent`] after processing a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// Which agent produced this output.
    pub role: AgentRole,
    /// Human-readable summary of what the agent did.
    pub summary: String,
    /// Arbitrary structured payload (e.g. a diff, a plan, a review report).
    pub payload: serde_json::Value,
    /// Whether the agent considers its sub-task complete.
    pub success: bool,
    /// Token usage for this agent's LLM call(s).  Used by the controller to
    /// populate observability stage spans.
    pub tokens: Option<TokenUsage>,
}

// ──────────────────────────────────────────────
// Agent trait
// ──────────────────────────────────────────────

/// Every participant in the HarnessCode pipeline must implement this trait.
///
/// Agents are intentionally stateless between calls; all state is passed in
/// via `context` and returned via [`AgentOutput`].
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// Returns the role this agent fulfils in the pipeline.
    fn role(&self) -> AgentRole;

    /// Execute the agent's task given the current `context` string.
    ///
    /// Returns an [`AgentOutput`] describing the result, or an [`AgentError`]
    /// if execution failed irrecoverably.
    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError>;
}

// ──────────────────────────────────────────────
// Shared helpers
// ──────────────────────────────────────────────

/// Try to parse `text` as JSON; if it fails, return `{"raw": "<text>"}`.
pub(crate) fn parse_json_or_wrap(text: &str) -> serde_json::Value {
    serde_json::from_str(text)
        .unwrap_or_else(|_| serde_json::json!({ "raw": text }))
}

/// Convenience constructor for a single-shot LLM completion (no tool use).
///
/// Returns the full [`LlmResponse`] so callers can record token usage.
pub(crate) async fn simple_complete(
    llm: &Arc<dyn LlmProvider>,
    system: &str,
    user: impl Into<String>,
) -> Result<crate::llm::LlmResponse, AgentError> {
    use crate::llm::LlmMessage;
    let messages = vec![
        LlmMessage::system(system),
        LlmMessage::user(user.into()),
    ];
    Ok(llm.complete(&messages).await?)
}
