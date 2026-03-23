//! Compactor agent — background context-compaction for long-lived sessions.
//!
//! After the Reviewer approves a pipeline run, the [`CompactorAgent`] is spawned
//! as a fire-and-forget background task.  It checks whether the accumulated
//! conversation turns outside the recent window exceed the token budget, and if so
//! sends a single LLM call to summarise them into a rolling [`compacted_summary`].
//!
//! The agent intentionally does **not** implement the [`Agent`] trait — it needs
//! direct access to the [`SessionStore`] as well as the LLM, which falls outside
//! the `execute(&str) -> AgentOutput` contract.
//!
//! [`compacted_summary`]: crate::memory::SessionMemory::compacted_summary

use crate::llm::{LlmMessage, LlmProvider};
use crate::memory::{needs_compaction, SessionMemory, SessionStore, RECENT_TURNS_WINDOW};
use std::sync::Arc;
use tracing::{info, warn};

/// Background agent that compresses older conversation turns into a rolling
/// LLM-generated summary, keeping prompt sizes bounded across long sessions.
pub struct CompactorAgent {
    pub llm: Arc<dyn LlmProvider>,
    pub store: Arc<dyn SessionStore>,
}

impl CompactorAgent {
    /// Perform best-effort compaction for `session_id`.
    ///
    /// Always re-reads the session from disk before mutating it so that
    /// concurrent pipeline runs do not clobber each other's writes.
    /// Any error is logged and swallowed — compaction is advisory and its
    /// failure must never surface to the user.
    pub async fn compact(&self, session_id: &str, snapshot: SessionMemory) {
        // Re-read the latest state; a concurrent request may have already
        // written a newer version since the snapshot was taken.
        let mut memory = match self.store.get_session(session_id).await {
            Ok(Some(m)) => m,
            Ok(None) => snapshot,
            Err(e) => {
                warn!(
                    session_id,
                    error = %e,
                    "Compactor: failed to reload session; using snapshot"
                );
                return;
            }
        };

        // Re-check after reload — another concurrent task may have already compacted.
        if !needs_compaction(&memory) {
            return;
        }

        let split_at = memory.conversation_turns.len() - RECENT_TURNS_WINDOW;
        let older_turns: Vec<_> = memory.conversation_turns.drain(..split_at).collect();

        let turns_text = older_turns
            .iter()
            .map(|t| {
                format!(
                    "User: {}\nAssistant: {}",
                    t.request.trim(),
                    t.response_summary.trim()
                )
            })
            .collect::<Vec<_>>()
            .join("\n---\n");

        let existing = memory.compacted_summary.as_deref().unwrap_or("(none)");

        let prompt = format!(
            "You are maintaining context memory for a coding assistant session. \
            Produce a concise but complete summary that preserves key decisions, \
            completed changes, user preferences, and important artefacts. \
            Prioritise specifics (file names, function names, chosen approaches) \
            over generalities. Do not exceed 400 words.\n\n\
            Previous summary:\n{existing}\n\n\
            New turns to incorporate:\n{turns_text}\n\n\
            Respond with the updated summary only, no preamble."
        );

        match self.llm.complete(&[LlmMessage::user(prompt)]).await {
            Ok(response) => {
                let new_summary = response.content.trim().to_string();
                info!(
                    session_id,
                    compacted_turns = older_turns.len(),
                    summary_len     = new_summary.len(),
                    "Conversation history compacted"
                );
                memory.compacted_summary = Some(new_summary);
                memory.touch();
                if let Err(e) = self.store.save_session(&memory).await {
                    warn!(
                        session_id,
                        error = %e,
                        "Compactor: failed to save compacted session"
                    );
                }
            }
            Err(e) => {
                // The drained older_turns are local to this function.
                // The on-disk session was never modified, so all turns are safe.
                warn!(
                    session_id,
                    error = %e,
                    "Compactor: LLM summarisation failed; session unmodified"
                );
            }
        }
    }
}
