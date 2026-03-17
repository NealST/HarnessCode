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
//! | [`commands::save_config_profile`] | Persist a profile into `~/.harnesscode/config.toml` |
//! | [`commands::get_run_history`]     | Read past run summaries from `.harnesscode/runs.jsonl` |
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

use harnesscode_core::controller::PipelineEvent;
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
}

// ──────────────────────────────────────────────
// Event payload types (frontend ↔ backend contract)
// ──────────────────────────────────────────────

/// Mirrors `PipelineEvent` for JSON serialisation to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PipelineEventDto {
    StageStarted { role: String },
    StageCompleted { role: String, summary: String, success: bool },
    PipelineFailed { error: String },
}

impl From<PipelineEvent> for PipelineEventDto {
    fn from(e: PipelineEvent) -> Self {
        match e {
            PipelineEvent::StageStarted { role } => Self::StageStarted {
                role: role.to_string(),
            },
            PipelineEvent::StageCompleted { output } => Self::StageCompleted {
                role: output.role.to_string(),
                summary: output.summary.clone(),
                success: output.success,
            },
            PipelineEvent::PipelineFailed { error } => Self::PipelineFailed { error },
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
            commands::get_run_history,
        ])
        .run(tauri::generate_context!())
        .expect("error while running HarnessCode desktop application");
}
