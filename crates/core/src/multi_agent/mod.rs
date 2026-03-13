//! # Multi-Agent Orchestration
//!
//! This module defines the core abstractions for HarnessCode's multi-agent pipeline:
//!
//! * [`AgentRole`] — enum identifying a specific role in the pipeline.
//! * [`Agent`] — async trait that every agent must implement.
//! * [`AgentOutput`] — the result produced by a single agent execution.
//! * [`Controller`] — cybernetic feedback loop that drives the pipeline to convergence.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use tracing::{info, warn};

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
    /// Validates the output, runs tests, and decides pass/fail.
    Reviewer,
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentRole::Planner => write!(f, "Planner"),
            AgentRole::Coder => write!(f, "Coder"),
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
// Concrete mock agents (used for testing / demos)
// ──────────────────────────────────────────────

/// A minimal Planner agent that produces a mock execution plan.
pub struct PlannerAgent;

#[async_trait::async_trait]
impl Agent for PlannerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Planner
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Planner, "Analysing task and building execution plan");
        let plan = serde_json::json!({
            "steps": [
                "Analyse existing codebase",
                "Identify files to modify",
                "Generate code changes",
                "Run sandboxed tests",
                "Verify output"
            ],
            "input_summary": context
        });
        Ok(AgentOutput {
            role: AgentRole::Planner,
            summary: format!("Plan generated for task: {}", context),
            payload: plan,
            success: true,
        })
    }
}

/// A minimal Coder agent that produces a mock code diff.
pub struct CoderAgent;

#[async_trait::async_trait]
impl Agent for CoderAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Coder
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Coder, "Generating code changes");
        let diff = serde_json::json!({
            "type": "code_diff",
            "files_changed": 1,
            "diff": "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-// TODO\n+// Implemented",
            "context": context
        });
        Ok(AgentOutput {
            role: AgentRole::Coder,
            summary: "Code diff generated".to_string(),
            payload: diff,
            success: true,
        })
    }
}

/// A minimal Reviewer agent that validates the coder's output.
pub struct ReviewerAgent;

#[async_trait::async_trait]
impl Agent for ReviewerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Reviewer
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Reviewer, "Reviewing generated changes");
        let report = serde_json::json!({
            "type": "review_report",
            "tests_passed": true,
            "lint_passed": true,
            "security_scan": "clean",
            "context": context
        });
        Ok(AgentOutput {
            role: AgentRole::Reviewer,
            summary: "Review complete — all checks passed".to_string(),
            payload: report,
            success: true,
        })
    }
}

// ──────────────────────────────────────────────
// Controller — cybernetic feedback loop
// ──────────────────────────────────────────────

/// The cybernetic controller that drives the multi-agent pipeline.
///
/// Implements a **TOTE** (Test-Operate-Test-Exit) loop:
/// 1. **Test** — Planner evaluates the current state vs. the desired goal.
/// 2. **Operate** — Coder applies changes.
/// 3. **Test** — Reviewer checks the result against success criteria.
/// 4. **Exit** — Loop terminates on success or after `max_retries` failures.
pub struct Controller {
    /// Maximum number of full pipeline retries before giving up.
    pub max_retries: usize,
}

impl Controller {
    /// Create a new [`Controller`] with the given retry limit.
    pub fn new(max_retries: usize) -> Self {
        Self { max_retries }
    }

    /// Run the full Planner → Coder → Reviewer pipeline for the given `prompt`.
    ///
    /// Returns a [`Vec`] of [`AgentOutput`] from each stage of the final
    /// successful (or last attempted) run.
    pub async fn run(&self, prompt: &str) -> Result<Vec<AgentOutput>, AgentError> {
        let planner = PlannerAgent;
        let coder = CoderAgent;
        let reviewer = ReviewerAgent;

        let mut attempt = 0;

        loop {
            attempt += 1;
            info!(attempt, "Starting pipeline run");

            // ── Stage 1: Planning ────────────────────────────────────────────
            let plan = planner.execute(prompt).await?;
            if !plan.success {
                warn!(role = %plan.role, "Planner reported failure; retrying");
                if attempt >= self.max_retries {
                    return Err(AgentError::MaxRetriesExceeded(self.max_retries));
                }
                continue;
            }

            // ── Stage 2: Coding ──────────────────────────────────────────────
            let context_for_coder = serde_json::to_string(&plan.payload)?;
            let code_output = coder.execute(&context_for_coder).await?;
            if !code_output.success {
                warn!(role = %code_output.role, "Coder reported failure; retrying");
                if attempt >= self.max_retries {
                    return Err(AgentError::MaxRetriesExceeded(self.max_retries));
                }
                continue;
            }

            // ── Stage 3: Review ──────────────────────────────────────────────
            let context_for_reviewer = serde_json::to_string(&code_output.payload)?;
            let review = reviewer.execute(&context_for_reviewer).await?;
            if !review.success {
                warn!(role = %review.role, "Reviewer failed; retrying pipeline");
                if attempt >= self.max_retries {
                    return Err(AgentError::MaxRetriesExceeded(self.max_retries));
                }
                continue;
            }

            // ── All stages passed ────────────────────────────────────────────
            info!(attempt, "Pipeline converged successfully");
            return Ok(vec![plan, code_output, review]);
        }
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_controller_runs_all_agents() {
        let controller = Controller::new(3);
        let outputs = controller.run("add a hello world function").await.unwrap();
        assert_eq!(outputs.len(), 3);
        assert!(outputs.iter().all(|o| o.success));
    }

    #[tokio::test]
    async fn test_agent_roles() {
        assert_eq!(PlannerAgent.role(), AgentRole::Planner);
        assert_eq!(CoderAgent.role(), AgentRole::Coder);
        assert_eq!(ReviewerAgent.role(), AgentRole::Reviewer);
    }
}
