mod context;

use crate::agents::AgentError;
use crate::llm::{LlmMessage, LlmProvider};
use crate::memory::long_term::MemoryCard;
use crate::memory::SessionMemory;
use crate::observability::TokenUsage;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

/// Background agent that transforms a [`SessionMemory`] snapshot into a
/// [`MemoryCard`] by calling the LLM with a distillation prompt.
///
/// Like [`CompactorAgent`], this agent intentionally does **not** implement the
/// [`Agent`] trait — it needs direct LLM access outside the normal pipeline.
/// Errors are logged and returned so that the caller (the scheduler) can
/// decide whether to proceed with other sessions.
pub struct MemoryDistillerAgent {
    pub llm: Arc<dyn LlmProvider>,
}

impl MemoryDistillerAgent {
    /// Distil `session` into a [`MemoryCard`].
    ///
    /// Returns `Ok((None, usage))` when the session carries no meaningful
    /// technical content (the LLM returns `{"title":""}`).
    pub async fn distill(
        &self,
        session: &SessionMemory,
    ) -> Result<(Option<MemoryCard>, TokenUsage), AgentError> {
        let input = build_input(session);

        let response = self
            .llm
            .complete(&[
                LlmMessage::system(context::SYSTEM_PROMPT),
                LlmMessage::user(input),
            ])
            .await?;

        let usage = TokenUsage::from(&response);
        let raw = response.content.trim().to_string();

        let card = parse_card(&raw, session)
            .map_err(|e| {
                warn!(
                    session_id = %session.session_id,
                    error = %e,
                    raw = %raw,
                    "MemoryDistillerAgent: failed to parse LLM response"
                );
                AgentError::Pipeline(format!("memory distiller parse error: {e}"))
            })?;

        if let Some(ref c) = card {
            info!(session_id = %session.session_id, title = %c.title, "Distilled memory card");
        } else {
            info!(session_id = %session.session_id, "Session has no distillable content — skipped");
        }

        Ok((card, usage))
    }
}

// ──────────────────────────────────────────────
// Input builder
// ──────────────────────────────────────────────

fn build_input(session: &SessionMemory) -> String {
    let turns_text = session
        .conversation_turns
        .iter()
        .map(|t| format!("Request: {}\nOutcome: {}", t.request.trim(), t.response_summary.trim()))
        .collect::<Vec<_>>()
        .join("\n---\n");

    serde_json::json!({
        "session_id": session.session_id,
        "effective_requests": session.effective_requests.join("\n"),
        "known_relevant_files": session.known_relevant_files.join(", "),
        "persistent_summary": session.persistent_summary,
        "compacted_summary": session.compacted_summary,
        "conversation_turns": turns_text,
    })
    .to_string()
}

// ──────────────────────────────────────────────
// Response parser
// ──────────────────────────────────────────────

fn parse_card(
    raw: &str,
    session: &SessionMemory,
) -> Result<Option<MemoryCard>, String> {
    let json_str = strip_fences(raw);
    let v: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("JSON parse: {e}"))?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .ok_or("missing 'title' field")?
        .trim()
        .to_string();

    // Empty title signals "nothing to record"
    if title.is_empty() {
        return Ok(None);
    }

    let problem = v
        .get("problem")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let solution = v
        .get("solution")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let key_patterns = json_string_array(&v, "key_patterns");
    let tags = json_string_array(&v, "tags");
    let affected_files = json_string_array(&v, "affected_files");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(Some(MemoryCard {
        id: format!("mem-{}", Uuid::new_v4().as_simple()),
        session_id: session.session_id.clone(),
        project_path: None, // filled in by the scheduler
        created_at_secs: now,
        title,
        problem,
        solution,
        key_patterns,
        tags,
        affected_files,
    }))
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(inner) = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
    {
        return inner.trim_end_matches("```").trim();
    }
    s
}

fn json_string_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
