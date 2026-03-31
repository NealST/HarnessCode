//! Nightly memory scheduler — distils session turns into long-term memory cards.
//!
//! Runs as a fire-and-forget background task (`tokio::spawn`).  Each day at the
//! configured UTC hour it scans all sessions, filters those with new content
//! since the last run, calls [`MemoryDistillerAgent`] on each, and persists the
//! resulting [`MemoryCard`]s.
//!
//! The scheduler intentionally never panics — every error is logged and swallowed
//! so that a distillation failure cannot affect the user-facing pipeline.

use crate::agents::memory_distiller::MemoryDistillerAgent;
use crate::memory::long_term::LongTermMemoryStore;
use crate::memory::SessionStore;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

// ──────────────────────────────────────────────
// RememberConfig
// ──────────────────────────────────────────────

/// Configuration for the nightly memory distillation scheduler.
#[derive(Debug, Clone)]
pub struct RememberConfig {
    /// Whether the nightly distillation is enabled at all.
    pub enabled: bool,
    /// Hour of day in UTC at which to trigger distillation (0–23).  Default: 3.
    pub schedule_hour: u8,
    /// Minute within the hour to trigger (0–59).  Default: 0.
    pub schedule_minute: u8,
    /// Maximum number of sessions to process per nightly run.
    /// Caps LLM usage for large deployments.
    pub max_sessions_per_run: usize,
}

impl Default for RememberConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule_hour: 3,
            schedule_minute: 0,
            max_sessions_per_run: 30,
        }
    }
}

// ──────────────────────────────────────────────
// MemoryScheduler
// ──────────────────────────────────────────────

/// Background task that wakes nightly and distils sessions into memory cards.
///
/// Spawn via `tokio::spawn(scheduler.run())` at application startup.
pub struct MemoryScheduler {
    session_store: Arc<dyn SessionStore>,
    long_term: Arc<LongTermMemoryStore>,
    distiller: MemoryDistillerAgent,
    config: RememberConfig,
}

impl MemoryScheduler {
    pub fn new(
        session_store: Arc<dyn SessionStore>,
        long_term: Arc<LongTermMemoryStore>,
        distiller: MemoryDistillerAgent,
        config: RememberConfig,
    ) -> Self {
        Self {
            session_store,
            long_term,
            distiller,
            config,
        }
    }

    /// Run the nightly scheduler loop forever.  Never returns under normal operation.
    pub async fn run(self) {
        if !self.config.enabled {
            info!("MemoryScheduler: disabled — skipping nightly distillation");
            return;
        }

        info!(
            schedule = %format!("{:02}:{:02} UTC", self.config.schedule_hour, self.config.schedule_minute),
            "MemoryScheduler started"
        );

        // `last_run_day` tracks the UTC day (epoch / 86400) of the most recent run.
        // Initialise to 0 so the first trigger fires immediately if the current
        // minute happens to fall in the scheduling window.
        let mut last_run_day: u64 = 0;

        loop {
            // Poll once per minute — low overhead, accurate to ±60 s.
            tokio::time::sleep(Duration::from_secs(60)).await;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let today = now / 86_400;
            let tod = now % 86_400; // seconds since midnight UTC
            let target = (self.config.schedule_hour as u64) * 3_600
                + (self.config.schedule_minute as u64) * 60;

            // Fire if we're within the 60-second window and haven't run today.
            if tod >= target && tod < target + 60 && today > last_run_day {
                last_run_day = today;
                info!("MemoryScheduler: starting nightly distillation");
                self.run_once().await;
            }
        }
    }

    /// Perform one distillation pass over all sessions with new content.
    async fn run_once(&self) {
        let mut index = match self.long_term.load_index().await {
            Ok(idx) => idx,
            Err(e) => {
                warn!(error = %e, "MemoryScheduler: failed to load distillation index");
                return;
            }
        };

        let sessions = match self.session_store.list_sessions().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "MemoryScheduler: failed to list sessions");
                return;
            }
        };

        // Only process sessions that have been updated since last distillation.
        let to_distill: Vec<String> = sessions
            .into_iter()
            .filter(|s| {
                let last = index.distilled_at.get(&s.session_id).copied().unwrap_or(0);
                s.updated_at_secs > last
            })
            .take(self.config.max_sessions_per_run)
            .map(|s| s.session_id)
            .collect();

        info!(count = to_distill.len(), "MemoryScheduler: sessions queued for distillation");

        for session_id in to_distill {
            let session = match self.session_store.get_session(&session_id).await {
                Ok(Some(s)) => s,
                Ok(None) => continue,
                Err(e) => {
                    warn!(session_id = %session_id, error = %e, "MemoryScheduler: failed to load session");
                    continue;
                }
            };

            match self.distiller.distill(&session).await {
                Ok((Some(mut card), _tokens)) => {
                    // Fill project_path from the long-term store's location.
                    card.project_path = Some(
                        self.long_term.project_dir().to_string_lossy().into_owned(),
                    );
                    info!(
                        session_id = %session_id,
                        title = %card.title,
                        "MemoryScheduler: card distilled"
                    );
                    if let Err(e) = self.long_term.save_card(&card).await {
                        warn!(session_id = %session_id, error = %e, "MemoryScheduler: failed to save card");
                    }
                }
                Ok((None, _)) => {
                    // Session had insufficient content — mark as processed to avoid retrying.
                    info!(session_id = %session_id, "MemoryScheduler: session skipped (no meaningful content)");
                }
                Err(e) => {
                    warn!(session_id = %session_id, error = %e, "MemoryScheduler: distillation failed");
                    // Do NOT update the index — allow a retry on the next run.
                    continue;
                }
            }

            index
                .distilled_at
                .insert(session_id, now_secs());
        }

        if let Err(e) = self.long_term.save_index(&index).await {
            warn!(error = %e, "MemoryScheduler: failed to persist distillation index");
        }

        info!("MemoryScheduler: nightly distillation complete");
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
