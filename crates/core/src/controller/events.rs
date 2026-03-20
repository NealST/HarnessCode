//! Pipeline events emitted by the Controller to external consumers (e.g. CLI spinner).

use crate::agents::{AgentOutput, AgentRole};
use serde::{Deserialize, Serialize};

/// Events emitted by [`super::Controller::run_with_progress`] as the pipeline runs.
///
/// Consumers receive these over a [`tokio::sync::mpsc`] channel and can drive
/// real-time UI (spinners, progress bars, log lines).
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// An agent stage has started — show a "thinking" indicator.
    StageStarted { role: AgentRole },
    /// The Judge decided how to route the current request.
    JudgeReady {
        route: String,
        route_reason_code: String,
        ready_for_scoper: bool,
        ready_for_planner: bool,
        ask_user_clarification: bool,
        effective_request: String,
        goal_is_concrete: bool,
        constraints_are_stable: bool,
        history_resolves_references: bool,
        repository_grounding_needed: bool,
        prior_scope_can_be_reused: bool,
        skip_scoper_criteria_met: Vec<String>,
        missing_information: Vec<String>,
        clarifying_questions: Vec<String>,
        confidence: String,
    },
    /// The Scoper finished framing the user's request.
    ScopeReady {
        task_type: String,
        objective: String,
        in_scope: Vec<String>,
        out_of_scope: Vec<String>,
        unknowns: Vec<String>,
        success_criteria: Vec<String>,
        relevant_files: Vec<String>,
        needs_user_clarification: bool,
        clarifying_questions: Vec<String>,
        confidence: String,
    },
    /// The pipeline is paused while waiting for the user to answer clarification questions.
    ClarificationRequested {
        source: AgentRole,
        objective: String,
        questions: Vec<String>,
    },
    /// The Planner produced an execution plan.
    ///
    /// Emitted immediately after [`StageCompleted`] for the Planner so that
    /// consumers (CLI, desktop UI) can display the upcoming action steps as a
    /// todo list before the Coder starts executing.
    PlanReady {
        steps: Vec<String>,
        affected_files: Vec<String>,
        complexity: String,
    },
    /// An agent stage completed successfully.
    StageCompleted { output: AgentOutput },
    /// The pipeline failed (agent error, guardrail, or max retries exceeded).
    PipelineFailed { error: String },
    /// A network or API error occurred during an LLM request.
    ///
    /// Informational — the controller may retry the stage.  Consumers should
    /// show a warning so users know why a stage is slow or failing.
    NetworkError {
        /// Error classification: `"request_timeout"`, `"connection_error"`,
        /// `"network_error"`, `"rate_limited"`, `"server_error"`, `"api_error"`.
        category: String,
        /// Human-readable message for the user.
        message: String,
        /// Which agent stage was running when the error occurred.
        role: AgentRole,
    },
}

/// Categorises why a pipeline run terminated before completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminationReason {
    MaxRetries,
    Timeout,
    GuardrailViolation(String),
}
