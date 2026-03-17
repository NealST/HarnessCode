//! [`TerminalSink`] — collects spans during a run and renders a tree summary at flush time.

use super::{Span, SpanKind, SpanStatus, SpanSink, TokenUsage};
use std::sync::Mutex;

/// Collects all spans in memory and on [`flush`](TerminalSink::flush) renders
/// a coloured tree summary to stdout.
///
/// The live spinner (driven by `PipelineEvent`) continues to show real-time
/// progress during execution; this sink only prints the final compact report.
pub struct TerminalSink {
    spans: Mutex<Vec<Span>>,
}

impl TerminalSink {
    pub fn new() -> Self {
        Self { spans: Mutex::new(Vec::new()) }
    }
}

impl Default for TerminalSink {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanSink for TerminalSink {
    fn record(&self, span: Span) {
        self.spans.lock().unwrap().push(span);
    }

    fn flush(&self) {
        // Clone the span list and release the lock before any I/O so
        // concurrent `record()` calls are never blocked by the print loop.
        let spans = self.spans.lock().unwrap().clone();
        if spans.is_empty() {
            return;
        }
        print_summary(&spans);
    }
}

// ──────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────

fn status_icon(status: &SpanStatus) -> &'static str {
    match status {
        SpanStatus::Ok => "✓",
        SpanStatus::Error { .. } => "✗",
        SpanStatus::Retried { .. } => "↺",
        SpanStatus::GuardrailTriggered { .. } => "⚠",
    }
}

fn fmt_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

fn fmt_tokens(t: Option<TokenUsage>) -> String {
    match t {
        Some(u) if u.total() > 0 => format!("  ↑{} ↓{} tok", u.prompt, u.completion),
        _ => String::new(),
    }
}

fn print_summary(spans: &[Span]) {
    // ── Find root pipeline span ───────────────────────────────────────────
    let pipeline = spans.iter().find(|s| matches!(s.kind, SpanKind::Pipeline { .. }));
    let Some(root) = pipeline else { return };

    let (prompt, total_dur) = match &root.kind {
        SpanKind::Pipeline { prompt } => (prompt.as_str(), root.duration),
        _ => ("", root.duration),
    };

    let run_short = root.run_id.short();
    let total_tokens: u32 = spans.iter()
        .filter_map(|s| s.tokens)
        .map(|t| t.total())
        .sum();

    // ── Header box ────────────────────────────────────────────────────────
    let width = 60usize;
    let header_line = format!("Pipeline Run  {}  {}", run_short, fmt_duration(total_dur));
    let padding = width.saturating_sub(header_line.len() + 4);
    println!();
    println!("╭─ {} {}─╮", header_line, "─".repeat(padding));
    // truncate prompt to fit
    let prompt_display: String = prompt.chars().take(width - 12).collect();
    println!("│  prompt  {:<width$}│", prompt_display, width = width - 11);

    let result_icon = if spans.iter().any(|s| matches!(s.status, SpanStatus::Error { .. })) {
        "❌"
    } else {
        "✅"
    };
    println!("│  result  {:<width$}│", format!("{result_icon}"), width = width - 11);
    println!("╰{}╯", "─".repeat(width + 2));
    println!();

    // ── Stage rows + tool turn / call children ────────────────────────────
    for span in spans.iter().filter(|s| matches!(s.kind, SpanKind::Stage { .. })) {
        let (role, _attempt) = match &span.kind {
            SpanKind::Stage { role, attempt } => (role, attempt),
            _ => continue,
        };
        let icon = status_icon(&span.status);
        let tok_str = fmt_tokens(span.tokens);
        println!(
            "  {}  {:<10}{:>8}{}",
            icon,
            role,
            fmt_duration(span.duration),
            tok_str,
        );
        if let SpanStatus::Error { message } = &span.status {
            println!("     └─ error: {message}");
        }
        if let SpanStatus::Retried { reason } = &span.status {
            println!("     ↺ retried: {reason}");
        }

        // Tool turns (children of this stage span)
        let turn_spans: Vec<&Span> = spans.iter()
            .filter(|s| s.parent_id == Some(span.span_id) && matches!(s.kind, SpanKind::ToolTurn { .. }))
            .collect();

        for (ti, turn) in turn_spans.iter().enumerate() {
            let turn_num = match &turn.kind { SpanKind::ToolTurn { turn } => *turn, _ => 0 };
            let is_last_turn = ti == turn_spans.len() - 1;
            let turn_prefix = if is_last_turn { "└─" } else { "├─" };
            let call_count = spans.iter()
                .filter(|s| s.parent_id == Some(turn.span_id))
                .count();
            println!(
                "     {}  Turn {}  {}  {} call{}",
                turn_prefix,
                turn_num,
                fmt_duration(turn.duration),
                call_count,
                if call_count == 1 { "" } else { "s" },
            );

            // Tool calls (children of this turn)
            let call_spans: Vec<&Span> = spans.iter()
                .filter(|s| s.parent_id == Some(turn.span_id))
                .collect();
            for (ci, call) in call_spans.iter().enumerate() {
                let is_last_call = ci == call_spans.len() - 1;
                let call_prefix = if is_last_call { "└─" } else { "├─" };
                let (tool, blocked) = match &call.kind {
                    SpanKind::ToolCall { tool, blocked, .. } => (tool.as_str(), *blocked),
                    _ => continue,
                };
                let icon = if blocked { "⚠" } else { status_icon(&call.status) };
                let guard_note = if blocked { "  [guardrail]" } else { "" };
                println!(
                    "          {}  {}  {:<18}{}{}",
                    call_prefix,
                    icon,
                    tool,
                    fmt_duration(call.duration),
                    guard_note,
                );
            }
        }
    }

    // ── Footer ────────────────────────────────────────────────────────────
    println!();
    println!("  {}", "─".repeat(width - 2));
    println!("  Total  {}  •  {} tokens", fmt_duration(total_dur), total_tokens);
    println!();
}
