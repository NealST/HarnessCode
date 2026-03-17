//! [`SpanSink`] trait and built-in sink implementations.

use super::Span;

// ──────────────────────────────────────────────
// Trait
// ──────────────────────────────────────────────

/// A destination that receives completed [`Span`]s.
///
/// All methods are synchronous so that sinks can be used from `Drop`-like
/// patterns and from non-async contexts.  Implementations that need I/O
/// (e.g. [`super::JsonLinesSink`]) buffer writes internally.
pub trait SpanSink: Send + Sync {
    /// Called once for every span that finishes.
    fn record(&self, span: Span);

    /// Called at the end of a run to flush any buffered data and (for
    /// [`super::TerminalSink`]) print the final summary.
    fn flush(&self) {}
}

// ──────────────────────────────────────────────
// NoopSink
// ──────────────────────────────────────────────

/// Discards every span. Used in tests and as a zero-cost default.
pub struct NoopSink;

impl SpanSink for NoopSink {
    fn record(&self, _span: Span) {}
}

// ──────────────────────────────────────────────
// CompositeSink
// ──────────────────────────────────────────────

/// Fan-out sink that forwards each span to multiple downstream sinks.
///
/// ```rust,ignore
/// let sink = CompositeSink::new(vec![
///     Arc::new(TerminalSink::new()),
///     Arc::new(JsonLinesSink::open(project_path)?),
/// ]);
/// ```
pub struct CompositeSink {
    sinks: Vec<std::sync::Arc<dyn SpanSink>>,
}

impl CompositeSink {
    pub fn new(sinks: Vec<std::sync::Arc<dyn SpanSink>>) -> Self {
        Self { sinks }
    }
}

impl SpanSink for CompositeSink {
    fn record(&self, span: Span) {
        for sink in &self.sinks {
            sink.record(span.clone());
        }
    }

    fn flush(&self) {
        for sink in &self.sinks {
            sink.flush();
        }
    }
}
