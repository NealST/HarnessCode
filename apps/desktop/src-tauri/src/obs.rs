//! [`TauriSpanSink`] — forwards every [`Span`] to the frontend as a Tauri event.
//!
//! The frontend subscribes to `"pipeline:span"` events and can render a live
//! span tree (token counts, tool-call timings, stage status) without polling.

use harnesscode_core::observability::{Span, SpanSink};
use tauri::{AppHandle, Emitter};
use tracing::warn;

/// Emits each completed span as a `"pipeline:span"` Tauri event.
///
/// Uses `AppHandle::emit` which fans out to all windows — suitable for a
/// single-window desktop app.
pub struct TauriSpanSink {
    pub app: AppHandle,
}

impl SpanSink for TauriSpanSink {
    fn record(&self, span: Span) {
        if let Err(e) = self.app.emit("pipeline:span", &span) {
            warn!("TauriSpanSink: failed to emit span event: {e}");
        }
    }
}
