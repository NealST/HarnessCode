//! Multi-session memory storage for long-lived conversation state.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs;
use uuid::Uuid;

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
    #[serde(default)]
    pub open_questions: Vec<String>,
    pub last_scope: Option<serde_json::Value>,
    pub last_plan: Option<serde_json::Value>,
}

impl SessionMemoryPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.execution_summary.is_none()
            && self.persistent_summary.is_none()
            && self.clarified_facts.is_empty()
            && self.effective_requests.is_empty()
            && self.known_relevant_files.is_empty()
            && self.open_questions.is_empty()
            && self.last_scope.is_none()
            && self.last_plan.is_none()
    }
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
            root: project_dir.as_ref().join(".harnesscode").join("memory").join("sessions"),
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
        self.ensure_dir().await?;
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
        fs::write(path, content).await?;
        Ok(())
    }

    async fn patch_session(
        &self,
        session_id: &str,
        patch: SessionMemoryPatch,
    ) -> Result<SessionMemory, MemoryError> {
        let mut memory = self.load_or_default(session_id).await?;
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
        merge_unique(&mut memory.effective_requests, patch.effective_requests);
        merge_unique(&mut memory.known_relevant_files, patch.known_relevant_files);
        memory.open_questions = patch.open_questions;
        if let Some(last_scope) = patch.last_scope {
            memory.last_scope = Some(last_scope);
        }
        if let Some(last_plan) = patch.last_plan {
            memory.last_plan = Some(last_plan);
        }
        memory.touch();
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
        self.ensure_dir().await?;
        let path = self.file_path(session_id);
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    async fn list_sessions(&self) -> Result<Vec<SessionMemorySummary>, MemoryError> {
        self.ensure_dir().await?;
        let mut entries = fs::read_dir(&self.root).await?;
        let mut sessions = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_file() {
                let content = fs::read_to_string(entry.path()).await?;
                let memory: SessionMemory = serde_json::from_str(&content)?;
                sessions.push(SessionMemorySummary {
                    session_id: memory.session_id,
                    title: memory.title,
                    updated_at_secs: memory.updated_at_secs,
                });
            }
        }

        sessions.sort_by(|a, b| b.updated_at_secs.cmp(&a.updated_at_secs));
        Ok(sessions)
    }
}

fn merge_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !value.trim().is_empty() && !target.iter().any(|existing| existing == &value) {
            target.push(value);
        }
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