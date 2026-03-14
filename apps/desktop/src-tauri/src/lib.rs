//! # HarnessCode Desktop — Tauri v2 Backend Library
//!
//! This crate exposes [`harnesscode_core`] functionality as Tauri commands
//! that the React frontend can `invoke()`.
//!
//! ## Commands
//!
//! | Command | Description |
//! |---------|-------------|
//! | [`invoke_agent_task`] | Run the full multi-agent pipeline for a given prompt |
//! | [`check_file_risk`]   | Assess the risk of a given file path |
//!
//! All commands are `async` and return serialisable JSON values so that the
//! frontend can render the appropriate Generative UI card.

use harnesscode_core::{
    multi_agent::{AgentOutput, Controller},
    risk_management::{RiskAssessment, RiskError, RiskManager},
};
use serde::{Deserialize, Serialize};
use tauri::command;
use tracing::{error, info};

// ──────────────────────────────────────────────
// Response types
// ──────────────────────────────────────────────

/// Discriminated-union response for the `invoke_agent_task` command.
/// The frontend renders different UI cards based on the `card_type` field.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "card_type", rename_all = "snake_case")]
pub enum AgentTaskResponse {
    /// Pipeline completed successfully — show a Code Diff Card.
    CodeDiff {
        outputs: Vec<AgentOutput>,
        summary: String,
    },
    /// A high-risk file was detected — show a Risk Alert Card.
    RiskAlert {
        filepath: String,
        reason: String,
        blocked: bool,
    },
    /// An unexpected error occurred.
    Error { message: String },
}

// ──────────────────────────────────────────────
// Tauri commands
// ──────────────────────────────────────────────

/// Run the full Planner → Coder → Reviewer pipeline for `prompt`.
///
/// Returns an [`AgentTaskResponse`] that the frontend uses to decide which
/// Generative UI card to render.
#[command]
pub async fn invoke_agent_task(prompt: String) -> AgentTaskResponse {
    info!(prompt = %prompt, "invoke_agent_task called from frontend");

    let controller = Controller::new(3);
    match controller.run(&prompt).await {
        Ok(outputs) => {
            let summary = format!(
                "Pipeline completed: {} stage(s) passed.",
                outputs.len()
            );
            AgentTaskResponse::CodeDiff { outputs, summary }
        }
        Err(e) => {
            error!("Controller failed: {e}");
            AgentTaskResponse::Error {
                message: e.to_string(),
            }
        }
    }
}

/// Assess the risk of modifying `filepath`.
///
/// Returns a [`RiskAssessment`] on success, or a [`AgentTaskResponse::RiskAlert`]
/// if the file is classified as high risk.
#[command]
pub async fn check_file_risk(filepath: String) -> AgentTaskResponse {
    info!(filepath = %filepath, "check_file_risk called from frontend");

    let rm = RiskManager::new();
    match rm.check_file_risk(&filepath) {
        Ok(assessment) => {
            // Represent a non-blocking assessment as a CodeDiff response
            // so the frontend knows it can proceed.
            AgentTaskResponse::RiskAlert {
                filepath: assessment.filepath,
                reason: assessment.reason,
                blocked: false,
            }
        }
        Err(RiskError::HighRiskBlocked { filepath, reason }) => {
            AgentTaskResponse::RiskAlert {
                filepath,
                reason,
                blocked: true,
            }
        }
    }
}

// ──────────────────────────────────────────────
// App builder — called by main.rs
// ──────────────────────────────────────────────

/// Build and configure the Tauri application.
///
/// This function is called from `main.rs` so that the app setup is tested
/// independently of the binary entry point.
pub fn run() {
    // Initialise tracing for the desktop process
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .with_target(false)
        .compact()
        .init();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![invoke_agent_task, check_file_risk])
        .run(tauri::generate_context!())
        .expect("error while running HarnessCode desktop application");
}
