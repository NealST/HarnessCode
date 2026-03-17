//! # HarnessCode Desktop — Tauri v2 Backend Library
//!
//! This crate exposes [`harnesscode_core`] functionality as Tauri commands
//! that the React frontend can `invoke()`.
//!
//! ## Commands
//!
//! | Command | Description |
//! |---------|-------------|
//! | [`invoke_agent_task`]   | Run the full multi-agent pipeline for a given prompt |
//! | [`get_config`]          | Read the resolved configuration for the settings panel |
//! | [`save_config_profile`] | Persist a profile into `~/.harnesscode/config.toml` |
//!
//! All commands are `async` and return serialisable JSON values so that the
//! frontend can render the appropriate Generative UI card.

use harnesscode_core::{
    config::{default_provider, load_config, user_config_path, HarnessConfig, ProfileConfig},
    multi_agent::{AgentOutput, Controller},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// Risk assessment is embedded in `outputs` as an `AgentRole::Risk` entry.
    CodeDiff {
        outputs: Vec<AgentOutput>,
        summary: String,
    },
    /// An unexpected error occurred.
    Error { message: String },
}

// ──────────────────────────────────────────────
// Config commands (for the GUI settings panel)
// ──────────────────────────────────────────────

/// DTO for a single profile as seen by the frontend.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProfileDto {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    /// Whether an API key is configured (never exposes the key itself).
    pub has_api_key: bool,
}

/// Response shape for `get_config`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigDto {
    pub default_profile: Option<String>,
    pub profiles: Vec<ProfileDto>,
}

/// Read the resolved user-level configuration for the settings panel.
/// The API key value is never sent to the frontend — only whether one is set.
#[command]
pub async fn get_config() -> ConfigDto {
    let cfg = load_config();
    let profiles = cfg
        .profiles
        .into_iter()
        .map(|(name, p)| ProfileDto {
            name,
            provider: p.provider,
            model: p.model,
            base_url: p.base_url,
            has_api_key: p.api_key.is_some(),
        })
        .collect();
    ConfigDto {
        default_profile: cfg.default_profile,
        profiles,
    }
}

/// DTO sent by the frontend when saving a profile.
/// The `api_key` field is optional; if absent the existing key is preserved.
#[derive(Debug, Deserialize)]
pub struct SaveProfileRequest {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub set_as_default: bool,
}

/// Persist a profile into `~/.harnesscode/config.toml`.
/// Creates the file and directory if they do not exist.
#[command]
pub async fn save_config_profile(req: SaveProfileRequest) -> Result<(), String> {
    // Load existing user config so we don't overwrite other profiles
    let user_path = user_config_path().ok_or("Cannot determine home directory")?;
    let mut cfg: HarnessConfig = if user_path.exists() {
        let content = std::fs::read_to_string(&user_path).map_err(|e| e.to_string())?;
        HarnessConfig::from_toml(&content).map_err(|e| e.to_string())?
    } else {
        HarnessConfig::default()
    };

    // Merge the incoming profile
    let existing_key = cfg.profiles.get(&req.name).and_then(|p| p.api_key.clone());
    cfg.profiles.insert(
        req.name.clone(),
        ProfileConfig {
            provider: req.provider,
            model: req.model,
            base_url: req.base_url,
            // Preserve existing key if the frontend didn't provide a new one
            api_key: req.api_key.filter(|k| !k.is_empty()).or(existing_key),
        },
    );

    if req.set_as_default {
        cfg.default_profile = Some(req.name);
    }

    // Write back
    if let Some(parent) = user_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&user_path, cfg.to_toml()).map_err(|e| e.to_string())?;
    info!(path = %user_path.display(), "Config saved");
    Ok(())
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

    let llm = match default_provider() {
        Ok(p) => p,
        Err(e) => {
            error!("LLM configuration error: {e}");
            return AgentTaskResponse::Error {
                message: format!("LLM configuration error: {e}"),
            };
        }
    };

    let controller = Controller::new(3, llm);
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
        .invoke_handler(tauri::generate_handler![
            invoke_agent_task,
            get_config,
            save_config_profile,
        ])
        .run(tauri::generate_context!())
        .expect("error while running HarnessCode desktop application");
}
