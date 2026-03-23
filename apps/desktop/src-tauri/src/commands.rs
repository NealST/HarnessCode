//! Tauri command handlers — kept in a separate module to avoid the E0255
//! macro-namespace conflict that arises when `#[tauri::command]` and
//! `generate_handler![]` are used in the same module scope.

use crate::{
    obs::TauriSpanSink,
    ConfigDto, PipelineDoneEvent, PipelineEventDto, PipelineState, ProfileDto, RunSummary,
    SaveProfileRequest, StageSummary,
};
use harnesscode_core::{
    agents::{
        DriftCallback, DriftDecision, DriftSignal,
    },
    config::{default_provider, load_config, user_config_path, HarnessConfig, ProfileConfig},
    controller::{
        ClarificationCallback, ClarificationRequest, ClarificationResolution, Controller,
        PipelineEvent, RequestContext,
    },
    memory::{FileSessionStore, SessionMemory, SessionMemoryPatch, SessionMemorySummary, SessionStore},
    observability::{CompositeSink, JsonLinesSink, SpanSink},
};
use std::{path::PathBuf, sync::Arc};
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{error, info};

/// Start the multi-agent pipeline for `prompt`.
///
/// Returns immediately; progress arrives via Tauri events:
/// - `"pipeline:event"` — per-stage progress
/// - `"pipeline:span"`  — per-span observability data
/// - `"pipeline:done"`  — final result
#[tauri::command]
pub async fn start_pipeline(
    app: AppHandle,
    state: State<'_, PipelineState>,
    prompt: String,
    request_context: Option<RequestContext>,
    project_dir: Option<String>,
    max_tool_turns: Option<usize>,
) -> Result<(), String> {
    info!(prompt = %prompt, "start_pipeline invoked");

    let mut request_context = request_context.unwrap_or_else(|| RequestContext::from_prompt(prompt.clone()));
    if request_context.session_id.is_none() {
        request_context.session_id = Some("default".to_string());
    }

    let llm = default_provider().map_err(|e| e.to_string())?;

    let project_path: PathBuf = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Composite sink: Tauri events + JSONL file.
    let tauri_sink: Arc<dyn SpanSink> = Arc::new(TauriSpanSink { app: app.clone() });
    let sink: Arc<dyn SpanSink> = match JsonLinesSink::open(&project_path) {
        Ok(jsonl) => Arc::new(CompositeSink::new(vec![tauri_sink, Arc::new(jsonl)])),
        Err(e) => {
            tracing::warn!("JsonLinesSink unavailable ({e}), Tauri-only observability");
            tauri_sink
        }
    };

    let controller = {
        let memory_store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::for_project(&project_path));
        let mut c = Controller::new(3, llm).with_obs(sink).with_memory(memory_store);
        // Frontend param > config file > default (100)
        let turns = max_tool_turns
            .or_else(|| load_config().max_tool_turns);
        if let Some(t) = turns {
            c = c.with_max_tool_turns(t);
        }
        c
    };

    // Build the drift callback — emits a `pipeline:event` with type
    // `drift_detected` and waits for the user to invoke `submit_drift_decision`.
    let drift_app = app.clone();
    let drift_callback: DriftCallback = Arc::new(move |signal| {
        let app = drift_app.clone();
        Box::pin(async move {
            let (kind, reason) = match &signal {
                DriftSignal::Drifted { kind, reason } => (kind.to_string(), reason.clone()),
                DriftSignal::Aligned => return DriftDecision::Ignore,
            };
            // Emit the drift event to the frontend.
            let dto = PipelineEventDto::DriftDetected { kind, reason };
            if let Err(e) = app.emit("pipeline:event", &dto) {
                tracing::warn!("emit drift_detected failed: {e}");
            }
            // Create a oneshot channel; store the sender in shared state.
            let (tx, rx) = tokio::sync::oneshot::channel::<DriftDecision>();
            {
                let state = app.state::<PipelineState>();
                *state.drift_decision_tx.lock().await = Some(tx);
            }
            // Await the user's decision — with a 10 minute safety timeout so
            // a lost frontend (tab closed, network drop) cannot hang the pipeline
            // indefinitely. On timeout we treat it as Stop to fail safe.
            tokio::time::timeout(std::time::Duration::from_secs(600), rx)
                .await
                .unwrap_or(Ok(DriftDecision::Stop))   // timeout → Stop
                .unwrap_or(DriftDecision::Stop)        // sender dropped → Stop
        })
    });

    let clarification_app = app.clone();
    let clarification_callback: ClarificationCallback = Arc::new(move |_request: ClarificationRequest| {
        let app = clarification_app.clone();
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel::<ClarificationResolution>();
            {
                let state = app.state::<PipelineState>();
                *state.clarification_tx.lock().await = Some(tx);
            }

            tokio::time::timeout(std::time::Duration::from_secs(600), rx)
                .await
                .unwrap_or(Ok(ClarificationResolution::Abort))
                .unwrap_or(ClarificationResolution::Abort)
        })
    });

    // Cancellation channel (best-effort; the pipeline checks for timeout guardrails).
    let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel::<()>();
    *state.cancel_tx.lock().await = Some(cancel_tx);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<PipelineEvent>(32);

    let app_clone = app.clone();
    tokio::spawn(async move {
        // Forward PipelineEvents to the frontend while the pipeline runs.
        let forwarder = {
            let app = app_clone.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    let dto: PipelineEventDto = event.into();
                    if let Err(e) = app.emit("pipeline:event", &dto) {
                        tracing::warn!("emit pipeline:event failed: {e}");
                    }
                }
            })
        };

        let result = controller
            .run_with_request_context(
                &request_context,
                Some(tx),
                Some(drift_callback),
                Some(clarification_callback),
            )
            .await;
        let _ = forwarder.await;

        let done = match result {
            Ok(outputs) => PipelineDoneEvent::Ok {
                stages: outputs
                    .iter()
                    .map(|o| StageSummary {
                        role: o.role.to_string(),
                        summary: o.summary.clone(),
                        success: o.success,
                    })
                    .collect(),
            },
            Err(e) => {
                error!("Pipeline failed: {e}");
                PipelineDoneEvent::Err { message: e.to_string() }
            }
        };

        if let Err(e) = app_clone.emit("pipeline:done", &done) {
            tracing::warn!("emit pipeline:done failed: {e}");
        }
    });

    Ok(())
}

/// Drop the cancellation sender — signals the pipeline to stop on the next
/// guardrail check (timeout boundary).
#[tauri::command]
pub async fn cancel_pipeline(state: State<'_, PipelineState>) -> Result<(), String> {
    *state.cancel_tx.lock().await = None;
    info!("cancel_pipeline: cancellation signal sent");
    Ok(())
}

/// Read past run summaries from `<project_dir>/.harnesscode/runs.jsonl`.
/// Returns Pipeline-kind spans parsed into lightweight summaries, newest first.
#[tauri::command]
pub async fn get_run_history(project_dir: Option<String>) -> Vec<RunSummary> {
    let path: PathBuf = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".harnesscode")
        .join("runs.jsonl");

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut summaries: Vec<RunSummary> = content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["kind"]["type"].as_str() == Some("pipeline"))
        .map(|v| {
            let run_id = v["run_id"].as_str().unwrap_or("").to_string();
            let prompt = v["kind"]["prompt"].as_str().unwrap_or("").to_string();
            let duration_ms = v["duration"]["secs"].as_u64().unwrap_or(0) * 1000
                + v["duration"]["nanos"].as_u64().unwrap_or(0) / 1_000_000;
            let total_tokens = v["tokens"]["prompt"].as_u64().unwrap_or(0) as u32
                + v["tokens"]["completion"].as_u64().unwrap_or(0) as u32;
            let success = v["status"]["type"].as_str() == Some("ok");
            let started_at_secs = v["started_at"]["secs_since_epoch"].as_u64().unwrap_or(0);
            RunSummary { run_id, prompt, duration_ms, total_tokens, success, started_at_secs }
        })
        .collect();

    summaries.sort_by(|a, b| b.started_at_secs.cmp(&a.started_at_secs));
    summaries
}

#[tauri::command]
pub async fn list_memory_sessions(project_dir: Option<String>) -> Result<Vec<SessionMemorySummary>, String> {
    let project_path = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let store = FileSessionStore::for_project(project_path);
    store.list_sessions().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_session_memory(
    session_id: String,
    project_dir: Option<String>,
) -> Result<SessionMemory, String> {
    let project_path = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let store = FileSessionStore::for_project(project_path);
    Ok(store
        .get_session(&session_id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| SessionMemory::new(session_id, None)))
}

#[tauri::command]
pub async fn save_session_memory(
    session_id: String,
    title: Option<String>,
    persistent_summary: Option<String>,
    project_dir: Option<String>,
) -> Result<SessionMemory, String> {
    let project_path = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let store = FileSessionStore::for_project(project_path);
    store
        .patch_session(
            &session_id,
            SessionMemoryPatch {
                title: title.filter(|value| !value.trim().is_empty()),
                persistent_summary,
                ..SessionMemoryPatch::default()
            },
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn clear_session_memory(
    session_id: String,
    project_dir: Option<String>,
) -> Result<SessionMemory, String> {
    let project_path = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let store = FileSessionStore::for_project(project_path);
    store.clear_session(&session_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_session_memory(
    session_id: String,
    project_dir: Option<String>,
) -> Result<(), String> {
    let project_path = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let store = FileSessionStore::for_project(project_path);
    store.delete_session(&session_id).await.map_err(|e| e.to_string())
}

/// Generate (or regenerate) `AGENTS.md` in the project directory.
///
/// Returns `true` when a pre-existing file was overwritten, `false` on a fresh
/// creation.  Overwrite confirmation is the caller's responsibility.
#[tauri::command]
pub async fn generate_agents_md(project_dir: Option<String>) -> Result<bool, String> {
    let project_path: PathBuf = project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let dest = project_path.join("AGENTS.md");
    let existed = dest.exists();
    let content = harnesscode_core::commands::generate_agents_md(&project_path);
    std::fs::write(&dest, content).map_err(|e| e.to_string())?;
    Ok(existed)
}

// ──────────────────────────────────────────────
// Config commands
// ──────────────────────────────────────────────

#[tauri::command]
pub async fn get_config() -> ConfigDto {
    let cfg = load_config();
    ConfigDto {
        default_profile: cfg.default_profile,
        max_tool_turns: cfg.max_tool_turns,
        profiles: cfg
            .profiles
            .into_iter()
            .map(|(name, p)| ProfileDto {
                name,
                provider: p.provider,
                model: p.model,
                base_url: p.base_url,
                has_api_key: p.api_key.is_some(),
            })
            .collect(),
    }
}

#[tauri::command]
pub async fn save_config_profile(req: SaveProfileRequest) -> Result<(), String> {
    let user_path = user_config_path().ok_or("Cannot determine home directory")?;
    let mut cfg: HarnessConfig = if user_path.exists() {
        let content = std::fs::read_to_string(&user_path).map_err(|e| e.to_string())?;
        HarnessConfig::from_toml(&content).map_err(|e| e.to_string())?
    } else {
        HarnessConfig::default()
    };

    let existing_key = cfg.profiles.get(&req.name).and_then(|p| p.api_key.clone());
    cfg.profiles.insert(
        req.name.clone(),
        ProfileConfig {
            provider: req.provider,
            model: req.model,
            base_url: req.base_url,
            api_key: req.api_key.filter(|k| !k.is_empty()).or(existing_key),
        },
    );
    if req.set_as_default {
        cfg.default_profile = Some(req.name);
    }

    if let Some(parent) = user_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&user_path, cfg.to_toml()).map_err(|e| e.to_string())?;
    info!(path = %user_path.display(), "Config saved");
    Ok(())
}

/// Save global settings (non-profile fields like `max_tool_turns`).
#[tauri::command]
pub async fn save_settings(max_tool_turns: Option<usize>) -> Result<(), String> {
    let user_path = user_config_path().ok_or("Cannot determine home directory")?;
    let mut cfg: HarnessConfig = if user_path.exists() {
        let content = std::fs::read_to_string(&user_path).map_err(|e| e.to_string())?;
        HarnessConfig::from_toml(&content).map_err(|e| e.to_string())?
    } else {
        HarnessConfig::default()
    };

    cfg.max_tool_turns = max_tool_turns;

    if let Some(parent) = user_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&user_path, cfg.to_toml()).map_err(|e| e.to_string())?;
    info!(max_tool_turns = ?max_tool_turns, "Settings saved");
    Ok(())
}

/// Resolve the pending drift-detection prompt with the user's decision.
///
/// `decision` must be one of `"stop"`, `"restart"`, or `"ignore"`.
/// Called by the frontend `DriftModal` when the user clicks a button.
#[tauri::command]
pub async fn submit_drift_decision(
    state: State<'_, PipelineState>,
    decision: String,
) -> Result<(), String> {
    let d = match decision.as_str() {
        "stop"    => DriftDecision::Stop,
        "restart" => DriftDecision::Restart,
        "ignore"  => DriftDecision::Ignore,
        other     => return Err(format!("Unknown drift decision: '{other}'")),
    };
    if let Some(tx) = state.drift_decision_tx.lock().await.take() {
        let _ = tx.send(d);
    }
    Ok(())
}

/// Resolve a pending clarification prompt with a freeform user response.
#[tauri::command]
pub async fn submit_clarification_response(
    state: State<'_, PipelineState>,
    response: Option<String>,
) -> Result<(), String> {
    let resolution = match response {
        Some(value) if !value.trim().is_empty() => ClarificationResolution::Answer(value),
        _ => ClarificationResolution::Abort,
    };
    if let Some(tx) = state.clarification_tx.lock().await.take() {
        let _ = tx.send(resolution);
    }
    Ok(())
}
