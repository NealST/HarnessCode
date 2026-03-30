//! Multi-session memory storage for long-lived conversation state.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs;
use tracing::warn;
use uuid::Uuid;

// Maximum number of entries retained in accumulating vec fields.
// Keeps session JSON small and token usage bounded when reading context into LLM prompts.
const MAX_EFFECTIVE_REQUESTS: usize = 20;
const MAX_CLARIFIED_FACTS: usize = 50;
const MAX_KNOWN_RELEVANT_FILES: usize = 100;
const MAX_CONVERSATION_TURNS: usize = 50;

/// How many of the most-recent turns are expanded into verbatim `recent_messages`.
/// Older turns beyond this window are compressed into `conversation_summary`.
pub const RECENT_TURNS_WINDOW: usize = 4;

/// Estimated-token threshold for the older turns (those outside `RECENT_TURNS_WINDOW`).
/// When the rough token count of compactable turns exceeds this value, the controller
/// spawns a background compaction task — mirroring how Claude Code manages context.
///
/// Approximation: 1 token ≈ 4 ASCII characters.
/// At ~350 chars/turn average: 6 000 × 4 / 350 ≈ 68 turns outside the window.
pub const COMPACTION_TRIGGER_TOKENS: usize = 6_000;

/// A single recorded exchange between the user and the agent pipeline.
/// Persisted inside [`SessionMemory`] so that conversation history survives
/// page reloads, CLI restarts, and the desktop ↔ CLI boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    /// The effective request the user submitted (post-Judge rewrite when applicable).
    pub request: String,
    /// A short plain-text summary of what the pipeline did (from the Reviewer output).
    pub response_summary: String,
    pub timestamp_secs: u64,
}

impl ConversationTurn {
    pub fn new(request: impl Into<String>, response_summary: impl Into<String>) -> Self {
        Self {
            request: request.into(),
            response_summary: response_summary.into(),
            timestamp_secs: now_secs(),
        }
    }
}

/// Persisted memory for one session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMemory {
    pub session_id: String,
    pub title: Option<String>,
    pub execution_summary: Option<String>,
    pub persistent_summary: Option<String>,
    #[serde(default)]
    pub clarified_facts: Vec<String>,
    #[serde(default)]
    pub effective_requests: Vec<String>,
    #[serde(default)]
    pub known_relevant_files: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    pub last_scope: Option<serde_json::Value>,
    pub last_plan: Option<serde_json::Value>,
    /// Ordered history of user↔agent exchanges. The controller appends one entry
    /// per successful pipeline run and reads these back on the next request to
    /// populate `recent_messages` and `conversation_summary` in the `RequestContext`.
    #[serde(default)]
    pub conversation_turns: Vec<ConversationTurn>,
    /// LLM-generated rolling summary of turns that have been compacted out of
    /// `conversation_turns`. Produced by the controller when the estimated token
    /// cost of older turns exceeds `COMPACTION_TRIGGER_TOKENS`.
    #[serde(default)]
    pub compacted_summary: Option<String>,
    /// Risk level recorded after the most recent successful pipeline run.
    /// One of `"low"`, `"medium"`, `"high"`, or `None` if never assessed.
    #[serde(default)]
    pub last_risk_level: Option<String>,
    pub created_at_secs: u64,
    pub updated_at_secs: u64,
}

impl SessionMemory {
    pub fn new(session_id: impl Into<String>, title: Option<String>) -> Self {
        let now = now_secs();
        Self {
            session_id: session_id.into(),
            title,
            execution_summary: None,
            persistent_summary: None,
            clarified_facts: Vec::new(),
            effective_requests: Vec::new(),
            known_relevant_files: Vec::new(),
            open_questions: Vec::new(),
            last_scope: None,
            last_plan: None,
            conversation_turns: Vec::new(),
            compacted_summary: None,
            last_risk_level: None,
            created_at_secs: now,
            updated_at_secs: now,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at_secs = now_secs();
    }
}

/// Lightweight listing metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMemorySummary {
    pub session_id: String,
    pub title: Option<String>,
    pub updated_at_secs: u64,
}

/// Incremental updates for a session memory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMemoryPatch {
    pub title: Option<String>,
    pub execution_summary: Option<String>,
    pub persistent_summary: Option<String>,
    #[serde(default)]
    pub clarified_facts: Vec<String>,
    #[serde(default)]
    pub effective_requests: Vec<String>,
    #[serde(default)]
    pub known_relevant_files: Vec<String>,
    /// `None` → no update;  `Some(vec![])` → clear the list;  `Some([...])` → replace.
    pub open_questions: Option<Vec<String>>,
    pub last_scope: Option<serde_json::Value>,
    pub last_plan: Option<serde_json::Value>,
    /// A completed exchange to append to the session's conversation history.
    pub new_conversation_turn: Option<ConversationTurn>,
    /// Replacement value for the LLM-generated compacted summary.
    /// Set by the controller after a successful compaction pass.
    pub compacted_summary: Option<String>,
    /// Atomic compaction payload produced by [`CompactorAgent`].
    ///
    /// Drains all turns with `timestamp_secs <= drain_up_to_timestamp` **and** installs
    /// `new_summary` in a single `patch_session` call, so the drain and the summary
    /// update are applied to the freshest on-disk state — avoiding a
    /// read-modify-write race with concurrent controller saves.
    pub compaction: Option<CompactionPatch>,
    /// Risk level from the completed pipeline run (`"low"`, `"medium"`, or `"high"`).
    /// `None` means the risk stage was skipped or unavailable; the stored value is unchanged.
    #[serde(default)]
    pub last_risk_level: Option<String>,
}

impl SessionMemoryPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.execution_summary.is_none()
            && self.persistent_summary.is_none()
            && self.clarified_facts.is_empty()
            && self.effective_requests.is_empty()
            && self.known_relevant_files.is_empty()
            && self.open_questions.is_none()
            && self.last_scope.is_none()
            && self.last_plan.is_none()
            && self.new_conversation_turn.is_none()
            && self.compacted_summary.is_none()
            && self.compaction.is_none()
            && self.last_risk_level.is_none()
    }
}

/// Payload carried by [`SessionMemoryPatch::compaction`].
///
/// The `drain_up_to_timestamp` field identifies the boundary: every stored
/// `ConversationTurn` whose `timestamp_secs` is **≤** this value is removed
/// from `conversation_turns`, and `new_summary` is installed as the new
/// `compacted_summary`.  Any turns added concurrently by the controller
/// (which always get a fresh `now_secs()` timestamp) survive untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPatch {
    /// LLM-generated rolling summary that replaces the drained turns.
    pub new_summary: String,
    /// Drain all `ConversationTurn`s with `timestamp_secs <= this value`.
    pub drain_up_to_timestamp: u64,
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("memory serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Abstract storage for multi-session memory.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionMemory>, MemoryError>;
    async fn save_session(&self, memory: &SessionMemory) -> Result<(), MemoryError>;
    async fn patch_session(
        &self,
        session_id: &str,
        patch: SessionMemoryPatch,
    ) -> Result<SessionMemory, MemoryError>;
    async fn clear_session(&self, session_id: &str) -> Result<SessionMemory, MemoryError>;
    async fn delete_session(&self, session_id: &str) -> Result<(), MemoryError>;
    async fn list_sessions(&self) -> Result<Vec<SessionMemorySummary>, MemoryError>;
}

/// JSON-file implementation of [`SessionStore`].
pub struct FileSessionStore {
    root: PathBuf,
}

impl FileSessionStore {
    pub fn for_project(project_dir: impl AsRef<Path>) -> Self {
        Self {
            root: project_dir.as_ref().join(".harness").join("memory").join("sessions"),
        }
    }

    fn file_path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{}.json", stable_file_stem(session_id)))
    }

    async fn ensure_dir(&self) -> Result<(), MemoryError> {
        fs::create_dir_all(&self.root).await?;
        Ok(())
    }

    async fn load_or_default(&self, session_id: &str) -> Result<SessionMemory, MemoryError> {
        Ok(self
            .get_session(session_id)
            .await?
            .unwrap_or_else(|| SessionMemory::new(session_id.to_string(), None)))
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionMemory>, MemoryError> {
        // No ensure_dir here — a missing directory simply means no session exists yet.
        let path = self.file_path(session_id);
        match fs::read_to_string(path).await {
            Ok(content) => Ok(Some(serde_json::from_str(&content)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn save_session(&self, memory: &SessionMemory) -> Result<(), MemoryError> {
        self.ensure_dir().await?;
        let path = self.file_path(&memory.session_id);
        let content = serde_json::to_string_pretty(memory)?;
        // Atomic write: first write to a .tmp side-car, then rename.
        // rename(2) on the same filesystem is atomic, so a crash mid-write
        // can never leave a truncated JSON file behind.
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, content).await?;
        fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    async fn patch_session(
        &self,
        session_id: &str,
        patch: SessionMemoryPatch,
    ) -> Result<SessionMemory, MemoryError> {
        let mut memory = self.load_or_default(session_id).await?;
        apply_patch(&mut memory, patch);
        self.save_session(&memory).await?;
        Ok(memory)
    }

    async fn clear_session(&self, session_id: &str) -> Result<SessionMemory, MemoryError> {
        let existing = self.load_or_default(session_id).await?;
        let cleared = SessionMemory::new(session_id.to_string(), existing.title);
        self.save_session(&cleared).await?;
        Ok(cleared)
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), MemoryError> {
        // No ensure_dir — deleting a non-existent file is a no-op.
        let path = self.file_path(session_id);
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    async fn list_sessions(&self) -> Result<Vec<SessionMemorySummary>, MemoryError> {
        // If the directory doesn't exist yet there are simply no sessions.
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut sessions = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !entry.file_type().await?.is_file() {
                continue;
            }
            // Skip in-flight .tmp side-cars from an interrupted atomic write.
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                continue;
            }
            let content = match fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to read session file; skipping");
                    continue;
                }
            };
            // Deserialise only the three lightweight fields; serde ignores the rest.
            // This avoids loading last_scope/last_plan payloads (potentially large) into memory.
            match serde_json::from_str::<SessionMemorySummary>(&content) {
                Ok(summary) => sessions.push(summary),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to parse session file; skipping");
                }
            }
        }

        sessions.sort_by(|a, b| b.updated_at_secs.cmp(&a.updated_at_secs));
        Ok(sessions)
    }
}

/// Apply a [`SessionMemoryPatch`] to an existing [`SessionMemory`] in place.
///
/// Exposed publicly so the controller can apply patches to its in-memory cached copy
/// and then call `save_session` directly, avoiding a redundant disk read.
pub fn apply_patch(memory: &mut SessionMemory, patch: SessionMemoryPatch) {
    if let Some(title) = patch.title {
        memory.title = Some(title);
    }
    if let Some(execution_summary) = patch.execution_summary {
        memory.execution_summary = Some(execution_summary);
    }
    if let Some(persistent_summary) = patch.persistent_summary {
        memory.persistent_summary = Some(persistent_summary);
    }
    merge_unique(&mut memory.clarified_facts, patch.clarified_facts);
    truncate_vec(&mut memory.clarified_facts, MAX_CLARIFIED_FACTS);
    merge_unique(&mut memory.effective_requests, patch.effective_requests);
    truncate_vec(&mut memory.effective_requests, MAX_EFFECTIVE_REQUESTS);
    merge_unique(&mut memory.known_relevant_files, patch.known_relevant_files);
    truncate_vec(&mut memory.known_relevant_files, MAX_KNOWN_RELEVANT_FILES);
    // open_questions: None means "no update"; Some([]) means "clear"; Some([...]) means "replace".
    if let Some(questions) = patch.open_questions {
        memory.open_questions = questions;
    }
    if let Some(last_scope) = patch.last_scope {
        memory.last_scope = Some(last_scope);
    }
    if let Some(last_plan) = patch.last_plan {
        memory.last_plan = Some(last_plan);
    }
    if let Some(turn) = patch.new_conversation_turn {
        memory.conversation_turns.push(turn);
        if memory.conversation_turns.len() > MAX_CONVERSATION_TURNS {
            let excess = memory.conversation_turns.len() - MAX_CONVERSATION_TURNS;
            memory.conversation_turns.drain(..excess);
        }
    }
    if let Some(summary) = patch.compacted_summary {
        memory.compacted_summary = Some(summary);
    }
    if let Some(level) = patch.last_risk_level {
        memory.last_risk_level = Some(level);
    }
    if let Some(c) = patch.compaction {
        // Drain all turns up to and including the timestamp boundary.
        // Turns added by concurrent controller saves (higher timestamps) are kept.
        memory.conversation_turns.retain(|t| t.timestamp_secs > c.drain_up_to_timestamp);
        memory.compacted_summary = Some(c.new_summary);
    }
    memory.touch();
}

/// Returns `true` when the older turns (those outside the recent window) contain
/// enough text that an LLM compaction pass should be triggered.
///
/// The token estimate is intentionally coarse: one token ≈ four ASCII characters.
pub fn needs_compaction(memory: &SessionMemory) -> bool {
    if memory.conversation_turns.len() <= RECENT_TURNS_WINDOW {
        return false;
    }
    let split_at = memory.conversation_turns.len() - RECENT_TURNS_WINDOW;
    let older_char_count: usize = memory.conversation_turns[..split_at]
        .iter()
        .map(|t| t.request.len() + t.response_summary.len())
        .sum();
    older_char_count / 4 > COMPACTION_TRIGGER_TOKENS
}

/// Merge `values` into `target`, skipping blanks and duplicates.  O(n) via HashSet.
fn merge_unique(target: &mut Vec<String>, values: Vec<String>) {
    let mut seen: HashSet<String> = target.iter().cloned().collect();
    for value in values {
        if !value.trim().is_empty() && seen.insert(value.clone()) {
            target.push(value);
        }
    }
}

/// Retain only the most-recent `max` items, dropping the oldest from the front.
fn truncate_vec(v: &mut Vec<String>, max: usize) {
    if v.len() > max {
        *v = v.split_off(v.len() - max);
    }
}

fn stable_file_stem(session_id: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, session_id.as_bytes()).to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_store_round_trips_sessions() {
        let temp = std::env::temp_dir().join(format!("harnesscode-memory-{}", Uuid::new_v4()));
        let store = FileSessionStore::for_project(&temp);

        let patched = store
            .patch_session(
                "default",
                SessionMemoryPatch {
                    persistent_summary: Some("User prefers the current controller direction".into()),
                    clarified_facts: vec!["Reuse the existing event contract".into()],
                    effective_requests: vec!["Update the desktop event wiring".into()],
                    ..SessionMemoryPatch::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(patched.session_id, "default");
        assert_eq!(patched.clarified_facts.len(), 1);

        let listed = store.list_sessions().await.unwrap();
        assert_eq!(listed.len(), 1);

        let loaded = store.get_session("default").await.unwrap().unwrap();
        assert_eq!(loaded.effective_requests.len(), 1);
    }
}