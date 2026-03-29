//! # HarnessCode Desktop — Tauri v2 Backend Library
//!
//! Exposes [`harnesscode_core`] functionality as Tauri commands.
//!
//! ## Commands
//!
//! | Command | Description |
//! |---------|-------------|
//! | [`commands::start_pipeline`]      | Kick off the multi-agent pipeline; streams progress via Tauri events |
//! | [`commands::cancel_pipeline`]     | Request cancellation of the running pipeline (best-effort) |
//! | [`commands::get_config`]          | Read the resolved configuration for the settings panel |
//! | [`commands::save_config_profile`] | Persist a profile into `~/.harness/config.toml` |
//! | [`commands::get_run_history`]     | Read past run summaries from `.harness/runs.jsonl` |
//! | [`commands::list_memory_sessions`] | List persisted session memories for the current project |
//! | [`commands::get_session_memory`]   | Read one persisted session memory |
//! | [`commands::list_skills`]          | List all user-invocable skills for the project |
//! | [`commands::invoke_skill_command`] | Render a skill body with argument substitution |
//!
//! ## Event flow for `start_pipeline`
//!
//! ```text
//! Frontend  invoke("start_pipeline", { prompt, project_dir })
//!              ↓
//! Rust      spawns tokio task — calls run_with_progress(prompt, Some(tx))
//!              ↓  per PipelineEvent
//!           app.emit("pipeline:event", event)
//!              ↓  on completion
//!           app.emit("pipeline:done",  PipelineDoneEvent)
//!              ↓  per Span (observability, via TauriSpanSink)
//!           app.emit("pipeline:span",  span)
//! ```

pub mod commands;
pub mod obs;

use harnesscode_core::agents::DriftDecision;
use harnesscode_core::controller::{ClarificationResolution, PipelineEvent};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

// ──────────────────────────────────────────────
// Shared app state
// ──────────────────────────────────────────────

/// Holds an optional cancellation sender for the currently running pipeline.
/// Only one pipeline runs at a time from the desktop UI.
#[derive(Default)]
pub struct PipelineState {
    pub cancel_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Resolves the drift-decision oneshot when the user submits a decision
    /// via [`commands::submit_drift_decision`].  `None` when no drift prompt
    /// is currently active.
    pub drift_decision_tx: Mutex<Option<tokio::sync::oneshot::Sender<DriftDecision>>>,
    /// Resolves a clarification request when the user submits additional context
    /// via [`commands::submit_clarification_response`].
    pub clarification_tx: Mutex<Option<tokio::sync::oneshot::Sender<ClarificationResolution>>>,
}

// ──────────────────────────────────────────────
// Event payload types (frontend ↔ backend contract)
// ──────────────────────────────────────────────

/// DTO for a single phase summary inside `PlanReady`.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseSummaryDto {
    pub phase_id: usize,
    pub title: String,
    pub step_count: usize,
    pub complexity: String,
}

/// Mirrors `PipelineEvent` for JSON serialisation to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PipelineEventDto {
    StageStarted { role: String },
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
    ClarificationRequested {
        source: String,
        objective: String,
        questions: Vec<String>,
    },
    /// The planner finished and produced a phased execution plan.
    PlanReady {
        phase_count: usize,
        phases: Vec<PhaseSummaryDto>,
        complexity: String,
    },
    /// The Conductor is starting a new execution phase.
    PhaseStarted { phase_id: usize, title: String, total_phases: usize },
    /// A phase completed successfully.
    PhaseCompleted {
        phase_id: usize,
        title: String,
        total_phases: usize,
        explanation: String,
        files_changed: usize,
    },
    /// A phase failed and is being retried.
    PhaseRetrying { phase_id: usize, title: String, reason: String, attempt: usize },
    /// A phase permanently failed.
    PhaseFailed { phase_id: usize, title: String, reason: String },
    StageCompleted { role: String, summary: String, success: bool },
    PipelineFailed { error: String },
    /// An agent stage was intentionally skipped by the user.
    StageSkipped { role: String },
    /// An agent stage failed and is being retried (in-stage, not a full pipeline retry).
    StageRetrying { role: String, reason: String, attempt: usize },
    /// Emitted directly by the drift callback (not via the mpsc channel).
    DriftDetected { kind: String, reason: String },
    /// A network or API error occurred during an LLM request.
    NetworkError {
        category: String,
        message: String,
        role: String,
    },
}

impl From<PipelineEvent> for PipelineEventDto {
    fn from(e: PipelineEvent) -> Self {
        match e {
            PipelineEvent::StageStarted { role } => Self::StageStarted {
                role: role.to_string().to_lowercase(),
            },
            PipelineEvent::JudgeReady {
                route,
                route_reason_code,
                ready_for_scoper,
                ready_for_planner,
                ask_user_clarification,
                effective_request,
                goal_is_concrete,
                constraints_are_stable,
                history_resolves_references,
                repository_grounding_needed,
                prior_scope_can_be_reused,
                skip_scoper_criteria_met,
                missing_information,
                clarifying_questions,
                confidence,
            } => Self::JudgeReady {
                route,
                route_reason_code,
                ready_for_scoper,
                ready_for_planner,
                ask_user_clarification,
                effective_request,
                goal_is_concrete,
                constraints_are_stable,
                history_resolves_references,
                repository_grounding_needed,
                prior_scope_can_be_reused,
                skip_scoper_criteria_met,
                missing_information,
                clarifying_questions,
                confidence,
            },
            PipelineEvent::ScopeReady {
                task_type,
                objective,
                in_scope,
                out_of_scope,
                unknowns,
                success_criteria,
                relevant_files,
                needs_user_clarification,
                clarifying_questions,
                confidence,
            } => Self::ScopeReady {
                task_type,
                objective,
                in_scope,
                out_of_scope,
                unknowns,
                success_criteria,
                relevant_files,
                needs_user_clarification,
                clarifying_questions,
                confidence,
            },
            PipelineEvent::ClarificationRequested {
                source,
                objective,
                questions,
            } => Self::ClarificationRequested {
                source: source.to_string().to_lowercase(),
                objective,
                questions,
            },
            PipelineEvent::PlanReady { phase_count, phases, complexity } =>
                Self::PlanReady {
                    phase_count,
                    phases: phases.into_iter().map(|p| PhaseSummaryDto {
                        phase_id: p.phase_id,
                        title: p.title,
                        step_count: p.step_count,
                        complexity: p.complexity,
                    }).collect(),
                    complexity,
                },
            PipelineEvent::PhaseStarted { phase_id, title, total_phases } =>
                Self::PhaseStarted { phase_id, title, total_phases },
            PipelineEvent::PhaseCompleted { phase_id, title, total_phases, explanation, files_changed } =>
                Self::PhaseCompleted { phase_id, title, total_phases, explanation, files_changed },
            PipelineEvent::PhaseRetrying { phase_id, title, reason, attempt } =>
                Self::PhaseRetrying { phase_id, title, reason, attempt },
            PipelineEvent::PhaseFailed { phase_id, title, reason } =>
                Self::PhaseFailed { phase_id, title, reason },
            PipelineEvent::StageCompleted { output } => Self::StageCompleted {
                role: output.role.to_string().to_lowercase(),
                summary: output.summary.clone(),
                success: output.success,
            },
            PipelineEvent::PipelineFailed { error } => Self::PipelineFailed { error },
            PipelineEvent::StageSkipped { role } => Self::StageSkipped {
                role: role.to_string().to_lowercase(),
            },
            PipelineEvent::StageRetrying { role, reason, attempt } => Self::StageRetrying {
                role: role.to_string().to_lowercase(),
                reason,
                attempt,
            },
            PipelineEvent::NetworkError { category, message, role } =>
                Self::NetworkError { category, message, role: role.to_string().to_lowercase() },
        }
    }
}

/// Final result emitted as `"pipeline:done"` when the task finishes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PipelineDoneEvent {
    Ok { stages: Vec<StageSummary> },
    Err { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct StageSummary {
    pub role: String,
    pub summary: String,
    pub success: bool,
}

// ──────────────────────────────────────────────
// Config DTOs
// ──────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProfileDto {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub has_api_key: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigDto {
    pub default_profile: Option<String>,
    pub max_tool_turns: Option<usize>,
    pub profiles: Vec<ProfileDto>,
}

/// DTO sent by the frontend when saving a profile.
#[derive(Debug, Deserialize)]
pub struct SaveProfileRequest {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub set_as_default: bool,
}

// ──────────────────────────────────────────────
// Run history DTO
// ──────────────────────────────────────────────

/// Lightweight summary of a past run for the history panel.
#[derive(Debug, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub prompt: String,
    /// Total duration in milliseconds.
    pub duration_ms: u64,
    /// Total tokens across all stages.
    pub total_tokens: u32,
    pub success: bool,
    /// Unix epoch seconds of the pipeline span start.
    pub started_at_secs: u64,
}

// ──────────────────────────────────────────────
// App builder
// ──────────────────────────────────────────────

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .with_target(false)
        .compact()
        .init();

    tauri::Builder::default()
        .manage(PipelineState::default())
        .invoke_handler(tauri::generate_handler![
            commands::start_pipeline,
            commands::cancel_pipeline,
            commands::get_config,
            commands::save_config_profile,
            commands::save_settings,
            commands::get_run_history,
            commands::list_memory_sessions,
            commands::get_session_memory,
            commands::save_session_memory,
            commands::clear_session_memory,
            commands::delete_session_memory,
            commands::generate_agents_md,
            commands::submit_drift_decision,
            commands::submit_clarification_response,
            commands::list_skills,
            commands::invoke_skill_command,
        ])
        .run(tauri::generate_context!())
        .expect("error while running HarnessCode desktop application");
}
