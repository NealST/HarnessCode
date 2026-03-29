//! Pipeline events emitted by the Controller to external consumers (e.g. CLI spinner).

use crate::agents::{AgentOutput, AgentRole};
use serde::{Deserialize, Serialize};

/// Lightweight summary of one phase shown in `PlanReady`.
#[derive(Debug, Clone)]
pub struct PhaseSummary {
    pub phase_id: usize,
    pub title: String,
    pub step_count: usize,
    pub complexity: String,
}

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
    /// consumers (CLI, desktop UI) can display the upcoming phases as a
    /// structured todo list before the Conductor starts executing.
    PlanReady {
        /// Total number of phases in the plan.
        phase_count: usize,
        /// Summary of each phase for UI display (phase_id, title, step count).
        phases: Vec<PhaseSummary>,
        complexity: String,
    },
    /// The Conductor is starting a new execution phase.
    PhaseStarted {
        phase_id: usize,
        title: String,
        total_phases: usize,
    },
    /// A phase completed successfully.
    PhaseCompleted {
        phase_id: usize,
        title: String,
        total_phases: usize,
        explanation: String,
        files_changed: usize,
        /// Precise list of files written or patched during this phase, tracked
        /// from tool calls rather than LLM self-report.
        affected_files: Vec<String>,
    },
    /// A phase failed and is being retried (in-phase retry, not a full pipeline retry).
    PhaseRetrying {
        phase_id: usize,
        title: String,
        reason: String,
        attempt: usize,
    },
    /// A phase failed after all retries — the pipeline will restart or abort.
    PhaseFailed {
        phase_id: usize,
        title: String,
        reason: String,
    },
    /// An agent stage completed successfully.
    StageCompleted { output: AgentOutput },
    /// The Risk agent produced a structured assessment.
    ///
    /// Emitted immediately after [`StageCompleted`] for the Risk stage so that
    /// consumers can display the full structured risk report without needing to
    /// parse raw JSON from the payload.
    RiskAssessed {
        risk_level: String,
        reason: String,
        affected_areas: Vec<String>,
        breaking_change: bool,
        security_implications: String,
        cr_focus: String,
        /// `true` when risk assessment could not be completed (LLM error / non-JSON).
        risk_unavailable: bool,
    },
    /// The pipeline failed (agent error, guardrail, or max retries exceeded).
    PipelineFailed { error: String },
    /// An agent stage was intentionally skipped by the user.
    StageSkipped { role: AgentRole },
    /// An agent stage failed and is being retried.
    ///
    /// Emitted once per in-stage retry attempt so consumers can update
    /// their UI (e.g. change the spinner label) without clearing the stage.
    /// `attempt` is the attempt that just failed (1-based), `reason` is the
    /// agent's own failure summary.
    StageRetrying { role: AgentRole, reason: String, attempt: usize },
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
