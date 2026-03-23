//! Compactor agent — background context-compaction for long-lived sessions.
//!
//! After the Reviewer approves a pipeline run, the [`CompactorAgent`] is spawned
//! as a fire-and-forget background task.  It checks whether the accumulated
//! conversation turns outside the recent window exceed the token budget, and if so
//! sends a single LLM call to summarise them into a rolling [`compacted_summary`].
//!
//! The compactor works from the **snapshot** passed by the controller (the state
//! just after the pipeline turn was persisted) for the LLM computation, then
//! installs the result through [`SessionStore::patch_session`] with a
//! [`CompactionPatch`].  Because `patch_session` re-reads the on-disk state before
//! writing, any turns added by a concurrent request are preserved — there is no
//! read-modify-write race.
//!
//! The agent intentionally does **not** implement the [`Agent`] trait — it needs
//! direct access to the [`SessionStore`] as well as the LLM, which falls outside
//! the `execute(&str) -> AgentOutput` contract.
//!
//! [`compacted_summary`]: crate::memory::SessionMemory::compacted_summary

use crate::llm::{LlmMessage, LlmProvider};
use crate::memory::{needs_compaction, CompactionPatch, SessionMemory, SessionMemoryPatch, SessionStore, RECENT_TURNS_WINDOW};
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
    /// Uses `snapshot` (the session state captured right after the pipeline turn
    /// was persisted) to determine which turns to compact and to build the LLM
    /// prompt.  The result is written through [`SessionStore::patch_session`],
    /// which re-reads the latest on-disk state before applying the
    /// [`CompactionPatch`] — ensuring that any turns added by a concurrent
    /// pipeline run are never lost.
    ///
    /// Any error is logged and swallowed — compaction is advisory and its
    /// failure must never surface to the user.
    pub async fn compact(&self, session_id: &str, snapshot: SessionMemory) {
        // Use the snapshot to decide whether compaction is warranted and to
        // build the prompt.  The snapshot already includes the turn that was
        // just persisted, so it is always at least as fresh as the on-disk state
        // at the moment of the spawn.
        if !needs_compaction(&snapshot) {
            return;
        }

        let split_at = snapshot.conversation_turns.len() - RECENT_TURNS_WINDOW;
        let older_turns = &snapshot.conversation_turns[..split_at];

        // The drain boundary: all turns with timestamp_secs <= cutoff_ts will be
        // removed by apply_patch when we call patch_session.  Turns added after
        // this snapshot (newer timestamps) are unaffected.
        let cutoff_ts = match older_turns.last() {
            Some(t) => t.timestamp_secs,
            None => return, // nothing to drain; shouldn't happen after needs_compaction check
        };

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

        let existing = snapshot.compacted_summary.as_deref().unwrap_or("(none)");

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
                // patch_session re-reads the freshest on-disk state before writing,
                // so any turns added by a concurrent pipeline request are kept.
                let patch = SessionMemoryPatch {
                    compaction: Some(CompactionPatch {
                        new_summary,
                        drain_up_to_timestamp: cutoff_ts,
                    }),
                    ..SessionMemoryPatch::default()
                };
                if let Err(e) = self.store.patch_session(session_id, patch).await {
                    warn!(
                        session_id,
                        error = %e,
                        "Compactor: failed to save compacted session"
                    );
                }
            }
            Err(e) => {
                warn!(
                    session_id,
                    error = %e,
                    "Compactor: LLM summarisation failed; session unmodified"
                );
            }
        }
    }
}
