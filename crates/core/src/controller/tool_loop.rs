//! Agentic ReAct tool loop with guardrail enforcement and observability.
//!
//! This is the deterministic half of the Harness Engineering model:
//! the LLM reasons and selects tools; we execute them with guaranteed fidelity
//! and enforce all safety limits via [`ExecutionGuard`].

use super::guardrails::{ExecutionGuard, StepStatus};
use crate::agents::drift_judge::{DriftDecision, DriftParams, DriftSignal, TurnSummary};
use crate::agents::AgentError;
use crate::llm::{LlmCompletion, LlmMessage, LlmProvider};
use crate::observability::{ObsCtx, SpanKind, SpanStatus, TokenUsage};
use crate::tools::{ToolRegistry, ToolResult};
use std::sync::Arc;
use tracing::warn;

/// Whether a file change was a full rewrite or a patch application.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    /// `write_file` — the entire file was replaced with new content.
    Write,
    /// `apply_diff` — a unified diff was applied to the existing file.
    Patch,
}

/// A record of a single file write or patch made during a tool loop execution.
///
/// Collected from successful `write_file` / `apply_diff` tool calls and returned
/// alongside the final text and token usage from [`run_tool_loop`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WrittenFile {
    /// Path of the file that was written or patched.
    pub path: String,
    /// How the file was modified.
    pub change_type: ChangeType,
    /// Actual content written (for `write`) or unified diff applied (for `patch`).
    /// Truncated to 5 000 characters to stay within LLM context budgets.
    pub change: String,
}

/// Run the agentic tool loop until the LLM produces a final text response.
///
/// Returns `(final_text, accumulated_token_usage, written_files)` where
/// `written_files` is the ordered list of [`WrittenFile`] records capturing every
/// successful `write_file` / `apply_diff` call made during this loop, including
/// the actual content or diff for downstream risk analysis.
///
/// ## Guardrail enforcement order (each iteration)
/// 1. `increment_turns`       — step budget (only on `NeedTools`)
/// 2. `sanitise_calls`        — dedup (silent removal of duplicates)
///
/// ## Drift detection
/// If `drift` is `Some`, the judge is called every `drift.config.check_interval`
/// turns.  The judge receives the last `window_size` [`TurnSummary`] objects.
/// If drift is detected the async `drift.callback` is awaited; the result
/// determines whether to abort, restart with a reinforced prompt, or continue.
///
/// ## Observability events emitted
/// * One **ToolTurn** span per `NeedTools` iteration.
/// * One **ToolCall** span per individual tool dispatch (including blocked ones).
pub async fn run_tool_loop(
    llm: &Arc<dyn LlmProvider>,
    mut messages: Vec<LlmMessage>,
    registry: &ToolRegistry,
    guard: &ExecutionGuard,
    obs: &ObsCtx,
    drift: Option<&DriftParams>,
) -> Result<(String, TokenUsage, Vec<WrittenFile>), AgentError> {
    let mut turn = 0usize;
    let mut total_tokens = TokenUsage::default();
    let mut turn_summaries: Vec<TurnSummary> = Vec::new();
    // Ordered record of every successful write_file / apply_diff call.
    // Preserves full history so Risk can see all changes, including multiple
    // edits to the same file within one phase.
    let mut written_files: Vec<WrittenFile> = Vec::new();

    loop {
        // Call the LLM.  On network/API failure, record an observability span
        // before propagating the error so that timeout / connection issues are
        // visible in the span tree and JSONL logs.
        let completion = match llm.complete_with_tools(&messages, &registry.defs()).await {
            Ok(c) => c,
            Err(llm_err) => {
                let err_timer = obs.start_span();
                let category = llm_err.error_category().to_string();
                let user_msg = llm_err.user_message();
                warn!(category = %category, "LLM request failed: {user_msg}");
                obs.record(err_timer.finish(
                    obs.run_id,
                    obs.current_span_id,
                    SpanKind::LlmRequest { turn: turn + 1, category },
                    SpanStatus::Error { message: user_msg },
                    None,
                ));
                return Err(llm_err.into());
            }
        };

        match completion {
            LlmCompletion::Done { text, prompt_tokens, completion_tokens } => {
                total_tokens.add(TokenUsage::new(
                    prompt_tokens.unwrap_or(0),
                    completion_tokens.unwrap_or(0),
                ));
                return Ok((text, total_tokens, written_files));
            }

            LlmCompletion::NeedTools { calls: raw_calls, prompt_tokens, completion_tokens } => {
                // ── Guardrail #1: step budget ──────────────────────────────
                // Checked here (inside NeedTools) so that the LLM's final
                // Done response is never blocked by the counter — only actual
                // tool-use rounds count against the turn budget.
                let step_status = guard.increment_turns()?;

                total_tokens.add(TokenUsage::new(
                    prompt_tokens.unwrap_or(0),
                    completion_tokens.unwrap_or(0),
                ));
                turn += 1;
                let turn_timer = obs.start_span();
                let turn_span_id = turn_timer.id;

                // ── Guardrail #2: deduplicate ─────────────────────────────
                let calls = guard.sanitise_calls(raw_calls);

                // Record the assistant's tool-call turn.
                messages.push(LlmMessage::assistant_tool_calls(calls.clone()));

                // ── Dispatch all calls concurrently ──────────────────────
                let dispatched = futures::future::join_all(
                    calls.iter().enumerate().map(|(i, call)| {
                        let call_timer = obs.start_span();
                        async move { (i, call_timer, registry.dispatch(call).await) }
                    })
                ).await;

                let mut results: Vec<ToolResult> = calls
                    .iter()
                    .map(|_| ToolResult { content: String::new(), is_error: false })
                    .collect();

                for (idx, call_timer, outcome) in dispatched {
                    let call = &calls[idx];
                    let is_err = outcome.is_error;
                    // Track files written or patched by actuator tools,
                    // capturing the actual content / diff for Risk analysis.
                    if !is_err {
                        if call.name == "write_file" {
                            if let (Some(path), Some(content)) = (
                                call.arguments["path"].as_str(),
                                call.arguments["content"].as_str(),
                            ) {
                                let change: String = content.chars().take(5_000).collect();
                                written_files.push(WrittenFile {
                                    path: path.to_string(),
                                    change_type: ChangeType::Write,
                                    change,
                                });
                            }
                        } else if call.name == "apply_diff" {
                            if let (Some(path), Some(diff)) = (
                                call.arguments["path"].as_str(),
                                call.arguments["diff"].as_str(),
                            ) {
                                let change: String = diff.chars().take(5_000).collect();
                                written_files.push(WrittenFile {
                                    path: path.to_string(),
                                    change_type: ChangeType::Patch,
                                    change,
                                });
                            }
                        }
                    }
                    obs.record(call_timer.finish(
                        obs.run_id,
                        Some(turn_span_id),
                        SpanKind::ToolCall {
                            tool: call.name.clone(),
                            call_id: call.id.clone(),
                            blocked: false,
                        },
                        if is_err {
                            SpanStatus::Error { message: outcome.content.clone() }
                        } else {
                            SpanStatus::Ok
                        },
                        None,
                    ));
                    results[idx] = outcome;
                }

                // Feed each result back as a tool-result message.
                for (call, result) in calls.iter().zip(results.iter()) {
                    messages.push(LlmMessage::tool_result(
                        &call.id,
                        &result.content,
                        result.is_error,
                    ));
                }

                // ── Guardrail #1b: budget warning ─────────────────────────
                // When 80 % of the step budget is consumed, inject a system
                // hint so the LLM starts wrapping up gracefully instead of
                // being hard-cut at the limit.
                if let StepStatus::Warning { used, remaining } = step_status {
                    warn!(used, remaining, "Step budget 80% consumed — injecting wrap-up hint");
                    messages.push(LlmMessage::system(format!(
                        "[SYSTEM] You have used {used} of your tool-call budget. \
                         Only {remaining} tool-call rounds remain before the hard limit. \
                         Please wrap up your current task, produce a final response, \
                         and avoid starting new exploratory work."
                    )));
                }

                // ── Emit ToolTurn span ────────────────────────────────────
                obs.record(turn_timer.finish(
                    obs.run_id,
                    obs.current_span_id,
                    SpanKind::ToolTurn { turn },
                    SpanStatus::Ok,
                    None,
                ));

                // ── Drift detection ───────────────────────────────────────
                if let Some(dp) = drift {
                    // LOGIC-4 fix: serialize only (tool_name, arguments) pairs for
                    // the snippet — previously the full ToolCall struct was serialised,
                    // which included internal fields irrelevant to the judge.
                    let args_json = serde_json::to_string(
                        &calls.iter()
                            .map(|c| serde_json::json!({ "tool": c.name, "args": c.arguments }))
                            .collect::<Vec<_>>(),
                    )
                    .unwrap_or_default();
                    // Single-pass truncation to ≤500 chars.
                    let truncated: String = args_json.chars().take(500).collect();
                    let snippet = if truncated.len() < args_json.len() {
                        format!("{truncated}…")
                    } else {
                        truncated
                    };
                    turn_summaries.push(TurnSummary {
                        turn_number: turn,
                        tools_called: calls.iter().map(|c| c.name.clone()).collect(),
                        call_args_snippet: snippet,
                    });

                    // BUG-2 fix: guard against check_interval == 0 (division-by-zero).
                    // LOGIC-5 fix: skip the judge call when window_size == 0 (nothing
                    // to evaluate).
                    let should_check = dp.config.check_interval > 0
                        && dp.config.window_size > 0
                        && turn % dp.config.check_interval == 0;

                    if should_check {
                        let window_start = turn_summaries
                            .len()
                            .saturating_sub(dp.config.window_size);
                        let window = &turn_summaries[window_start..];

                        // DESIGN-3: wrap the judge LLM call in an observability span.
                        let judge_timer = obs.start_span();
                        // BUG-1 fix: use `judge_prompt` (phase-scoped) for the LLM call,
                        // keep `original_prompt` clean for use in the reinforced restart.
                        let judge_result = dp.judge.judge(&dp.judge_prompt, window).await;
                        let judge_status = match &judge_result {
                            Ok(_)  => SpanStatus::Ok,
                            Err(e) => SpanStatus::Error { message: e.to_string() },
                        };
                        obs.record(judge_timer.finish(
                            obs.run_id,
                            obs.current_span_id,
                            SpanKind::LlmRequest { turn, category: "drift_judge".into() },
                            judge_status,
                            None,
                        ));

                        match judge_result {
                            Ok(DriftSignal::Aligned) => {
                                // On track — continue normally.
                            }
                            Ok(signal @ DriftSignal::Drifted { .. }) => {
                                warn!(turn, "Drift detected — invoking callback for user decision");
                                let (kind, reason) = match &signal {
                                    DriftSignal::Drifted { kind, reason } => (kind.clone(), reason.clone()),
                                    DriftSignal::Aligned => unreachable!(),
                                };
                                // LOGIC-3 / MISSING-1: fire the synchronous notify hook
                                // *before* blocking on the async callback so the UI can
                                // immediately show a DriftDetected indicator.
                                if let Some(ref notify) = dp.notify {
                                    notify(&kind, &reason);
                                }
                                let decision = (dp.callback)(signal).await;
                                match decision {
                                    DriftDecision::Stop => {
                                        return Err(AgentError::DriftAborted);
                                    }
                                    DriftDecision::Restart => {
                                        // BUG-1 fix: use the clean `original_prompt` (not
                                        // the phase-scoped `judge_prompt`) so the reinforced
                                        // restart reflects the true user intent, not a
                                        // phase-specific objective.
                                        let reinforced = format!(
                                            "Original goal: {original}\n\nDrift detected during execution: {reason}\n\n\
Keep all actions in this new attempt strictly aligned with the original goal. \
Do not introduce changes unrelated to the original goal.",
                                            original = dp.original_prompt,
                                            reason = reason,
                                        );
                                        return Err(AgentError::DriftRestart {
                                            reinforced_prompt: reinforced,
                                        });
                                    }
                                    DriftDecision::Ignore => {
                                        // User chose to continue; proceed normally.
                                    }
                                }
                            }
                            Err(e) => {
                                // Judge failure is non-fatal — log and continue.
                                warn!(error = %e, "Drift judge failed (non-fatal, continuing)");
                            }
                        }

                        // LOGIC-6 fix: cap turn_summaries to the last window_size entries
                        // after each check so the vec doesn't grow unboundedly over a
                        // long-running phase.
                        let keep = dp.config.window_size;
                        if turn_summaries.len() > keep {
                            let drain_end = turn_summaries.len() - keep;
                            turn_summaries.drain(..drain_end);
                        }
                    }
                }
            }
        }
    }
}
