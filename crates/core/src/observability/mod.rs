//! # Observability
//!
//! Structured execution tracing for the HarnessCode multi-agent pipeline.
//!
//! Every significant unit of work — from a full pipeline run down to a single
//! tool call — is recorded as a [`Span`] and pushed to whatever [`SpanSink`]
//! the caller has configured.
//!
//! ## Span hierarchy
//!
//! ```text
//! Pipeline (run_id, prompt, total_duration)
//!   └── Stage: Planner   (duration, tokens_in, tokens_out, status)
//!   └── Stage: Coder     (duration, tokens_in, tokens_out, status)
//!         └── ToolTurn #1  (duration, n_calls)
//!               └── ToolCall: read_file  (duration, ok/error/blocked)
//!               └── ToolCall: run_command
//!         └── ToolTurn #2  …
//!   └── Stage: Risk
//!   └── Stage: Reviewer
//! ```
//!
//! ## Usage
//!
//! Build an [`ObsCtx`] at the start of a run and pass it into the controller.
//! The context is cheaply clonable (`Arc` inside) and carries the current
//! `run_id` + `parent_span_id` so child spans can reference their parent.
//!
//! ```rust,ignore
//! let sink = Arc::new(TerminalSink::new());
//! let obs  = ObsCtx::new_run(sink, "add hello world");
//! controller.run_with_obs("add hello world", obs, None).await?;
//! sink.flush();   // prints the summary tree to stdout
//! ```

pub mod jsonl;
pub mod sink;
pub mod terminal;

pub use jsonl::JsonLinesSink;
pub use sink::{CompositeSink, NoopSink, SpanSink};
pub use terminal::TerminalSink;

use crate::agents::AgentRole;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

// ──────────────────────────────────────────────
// IDs
// ──────────────────────────────────────────────

/// Opaque identifier for a single pipeline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(Uuid);

/// Opaque identifier for one span within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpanId(Uuid);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
    /// Returns the short 8-char hex prefix used in display output.
    pub fn short(&self) -> String {
        self.0.to_string()[..8].to_string()
    }
}

impl SpanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RunId {
    fn default() -> Self { Self::new() }
}

impl Default for SpanId {
    fn default() -> Self { Self::new() }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ──────────────────────────────────────────────
// Token usage
// ──────────────────────────────────────────────

/// Token consumption for one LLM call or aggregated across a stage.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
}

impl TokenUsage {
    pub fn new(prompt: u32, completion: u32) -> Self {
        Self { prompt, completion }
    }

    pub fn total(&self) -> u32 {
        self.prompt + self.completion
    }

    /// Merge another usage record into this one.
    pub fn add(&mut self, other: TokenUsage) {
        self.prompt += other.prompt;
        self.completion += other.completion;
    }
}

impl From<&crate::llm::LlmResponse> for TokenUsage {
    fn from(r: &crate::llm::LlmResponse) -> Self {
        Self {
            prompt: r.prompt_tokens.unwrap_or(0),
            completion: r.completion_tokens.unwrap_or(0),
        }
    }
}

// ──────────────────────────────────────────────
// Span kinds
// ──────────────────────────────────────────────

/// What kind of work a span represents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpanKind {
    /// The root span covering the entire pipeline run.
    Pipeline { prompt: String },
    /// One agent stage (Planner / Coder / Risk / Reviewer).
    Stage { role: AgentRole, attempt: usize },
    /// One iteration of the ReAct tool loop inside the Coder.
    ToolTurn { turn: usize },
    /// One individual tool invocation dispatched to the registry.
    ToolCall {
        tool: String,
        call_id: String,
        /// Whether this call was blocked by a guardrail (rate-limit exceeded).
        blocked: bool,
    },
    /// An LLM HTTP request that resulted in a network or API error.
    LlmRequest {
        /// Which tool-loop iteration was running (0 if before the first turn).
        turn: usize,
        /// Error classification: "request_timeout", "connection_error", etc.
        category: String,
    },
}

// ──────────────────────────────────────────────
// Span status
// ──────────────────────────────────────────────

/// Outcome of a span.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpanStatus {
    Ok,
    Error { message: String },
    /// The span completed but a retry was subsequently triggered.
    Retried { reason: String },
    /// A guardrail violation occurred and was handled (non-fatal).
    GuardrailTriggered { violation: String },
    /// The span was intentionally skipped (e.g. Risk bypassed because no file changes).
    Skipped { reason: String },
}

// ──────────────────────────────────────────────
// Span
// ──────────────────────────────────────────────

/// A single unit of observable work, analogous to an OpenTelemetry span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub run_id: RunId,
    pub span_id: SpanId,
    /// Parent span in the hierarchy.  `None` only for the root Pipeline span.
    pub parent_id: Option<SpanId>,
    pub kind: SpanKind,
    /// Wall-clock time at span start (for JSONL / cross-process ordering).
    pub started_at: SystemTime,
    pub duration: Duration,
    pub status: SpanStatus,
    /// Token usage (populated for Stage spans and the Pipeline root).
    pub tokens: Option<TokenUsage>,
    /// Arbitrary key-value annotations.
    pub attrs: HashMap<String, serde_json::Value>,
}

// ──────────────────────────────────────────────
// SpanTimer — RAII helper that measures elapsed time
// ──────────────────────────────────────────────

/// Lightweight timer that captures start wall-time and monotonic instant.
///
/// Call [`SpanTimer::finish`] to build a complete [`Span`].
pub struct SpanTimer {
    pub id: SpanId,
    started_at: SystemTime,
    instant: Instant,
}

impl SpanTimer {
    pub fn start() -> Self {
        Self {
            id: SpanId::new(),
            started_at: SystemTime::now(),
            instant: Instant::now(),
        }
    }

    pub fn finish(
        self,
        run_id: RunId,
        parent_id: Option<SpanId>,
        kind: SpanKind,
        status: SpanStatus,
        tokens: Option<TokenUsage>,
    ) -> Span {
        Span {
            run_id,
            span_id: self.id,
            parent_id,
            kind,
            started_at: self.started_at,
            duration: self.instant.elapsed(),
            status,
            tokens,
            attrs: HashMap::new(),
        }
    }
}

// ──────────────────────────────────────────────
// ObsCtx — lightweight context handle
// ──────────────────────────────────────────────

/// Observability context threaded through the pipeline.
///
/// Cheaply clonable via `Arc`.  Each level of the call hierarchy creates a
/// child context (with the current span as parent) before passing it deeper.
#[derive(Clone)]
pub struct ObsCtx {
    pub run_id: RunId,
    /// The span ID of the immediately enclosing span — used as `parent_id`
    /// when recording child spans.
    pub current_span_id: Option<SpanId>,
    sink: Arc<dyn SpanSink>,
}

impl ObsCtx {
    /// Create the root context for a new pipeline run.
    pub fn new_run(sink: Arc<dyn SpanSink>) -> Self {
        Self {
            run_id: RunId::new(),
            current_span_id: None,
            sink,
        }
    }

    /// Create a no-op context (useful in tests / when observability is disabled).
    pub fn noop() -> Self {
        Self::new_run(Arc::new(NoopSink))
    }

    /// Derive a child context where `span_id` becomes the new parent.
    pub fn child(&self, span_id: SpanId) -> Self {
        Self {
            run_id: self.run_id,
            current_span_id: Some(span_id),
            sink: Arc::clone(&self.sink),
        }
    }

    /// Record a completed span to the sink.
    pub fn record(&self, span: Span) {
        self.sink.record(span);
    }

    /// Helper: start a timer whose span will be attributed to the current
    /// span as parent.
    pub fn start_span(&self) -> SpanTimer {
        SpanTimer::start()
    }

    /// Flush the underlying sink (prints terminal summary, closes file, etc.).
    /// Call once at the end of a run.
    pub fn flush(&self) {
        self.sink.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_add() {
        let mut a = TokenUsage::new(100, 50);
        a.add(TokenUsage::new(200, 75));
        assert_eq!(a.prompt, 300);
        assert_eq!(a.completion, 125);
        assert_eq!(a.total(), 425);
    }

    #[test]
    fn span_timer_records_duration() {
        let obs = ObsCtx::noop();
        let timer = obs.start_span();
        let span = timer.finish(obs.run_id, None, SpanKind::Pipeline { prompt: "test".into() }, SpanStatus::Ok, None);
        // Duration should be near-zero but non-negative.
        assert!(span.duration.as_secs() < 5);
    }
}
