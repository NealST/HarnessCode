//! Agentic ReAct tool loop with guardrail enforcement and observability.
//!
//! This is the deterministic half of the Harness Engineering model:
//! the LLM reasons and selects tools; we execute them with guaranteed fidelity
//! and enforce all safety limits via [`ExecutionGuard`].

use super::guardrails::ExecutionGuard;
use crate::agents::AgentError;
use crate::llm::{LlmCompletion, LlmMessage, LlmProvider};
use crate::observability::{ObsCtx, SpanKind, SpanStatus, TokenUsage};
use crate::tools::{ToolRegistry, ToolResult};
use std::sync::Arc;

/// Run the agentic tool loop until the LLM produces a final text response.
///
/// Returns `(final_text, accumulated_token_usage)`.
///
/// ## Guardrail enforcement order (each iteration)
/// 1. `check_timeout`         — wall-clock deadline
/// 2. `increment_turns`       — step budget
/// 3. `sanitise_calls`        — dedup + concurrent ceiling (silent clamp)
/// 4. `check_and_record_tool` — per-tool rate limit (ToolResult::err, non-fatal)
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
) -> Result<(String, TokenUsage), AgentError> {
    let mut turn = 0usize;
    let mut total_tokens = TokenUsage::default();

    loop {
        // ── Guardrail #1 & #2 ─────────────────────────────────────────────
        guard.check_timeout()?;
        guard.increment_turns()?;

        match llm.complete_with_tools(&messages, &registry.defs()).await? {
            LlmCompletion::Done { text, prompt_tokens, completion_tokens } => {
                total_tokens.add(TokenUsage::new(
                    prompt_tokens.unwrap_or(0),
                    completion_tokens.unwrap_or(0),
                ));
                return Ok((text, total_tokens));
            }

            LlmCompletion::NeedTools { calls: raw_calls, prompt_tokens, completion_tokens } => {
                total_tokens.add(TokenUsage::new(
                    prompt_tokens.unwrap_or(0),
                    completion_tokens.unwrap_or(0),
                ));
                turn += 1;
                let turn_timer = obs.start_span();
                let turn_span_id = turn_timer.id;

                // ── Guardrail #3: deduplicate + clamp ─────────────────────
                let calls = guard.sanitise_calls(raw_calls);

                // Record the assistant's tool-call turn.
                messages.push(LlmMessage::assistant_tool_calls(calls.clone()));

                // ── Guardrail #4 + dispatch ──────────────────────────────
                // Phase A: pre-check rate limits for every call (serial, atomic).
                let mut approved: std::collections::HashSet<usize> = std::collections::HashSet::with_capacity(calls.len());
                let mut results: Vec<ToolResult> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, call)| match guard.check_and_record_tool(&call.name) {
                        Ok(()) => {
                            approved.insert(i);
                            ToolResult { content: String::new(), is_error: false }
                        }
                        Err(violation) => ToolResult::err(format!(
                            "Tool call blocked by guardrail: {violation}"
                        )),
                    })
                    .collect();

                // Phase B: dispatch all approved calls concurrently.
                let dispatched = futures::future::join_all(
                    approved.iter().map(|&i| {
                        let call_timer = obs.start_span();
                        let call = &calls[i];
                        async move { (i, call_timer, registry.dispatch(call).await) }
                    })
                ).await;
                for (idx, call_timer, outcome) in dispatched {
                    let call = &calls[idx];
                    let is_err = outcome.is_error;
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

                // Emit ToolCall spans for guardrail-blocked calls too.
                for (i, call) in calls.iter().enumerate() {
                    if results[i].is_error && !approved.contains(&i) {
                        let blocked_timer = obs.start_span();
                        obs.record(blocked_timer.finish(
                            obs.run_id,
                            Some(turn_span_id),
                            SpanKind::ToolCall {
                                tool: call.name.clone(),
                                call_id: call.id.clone(),
                                blocked: true,
                            },
                            SpanStatus::GuardrailTriggered {
                                violation: results[i].content.clone(),
                            },
                            None,
                        ));
                    }
                }

                // Feed each result back as a tool-result message.
                for (call, result) in calls.iter().zip(results.iter()) {
                    messages.push(LlmMessage::tool_result(
                        &call.id,
                        &result.content,
                        result.is_error,
                    ));
                }

                // ── Emit ToolTurn span ────────────────────────────────────
                obs.record(turn_timer.finish(
                    obs.run_id,
                    obs.current_span_id,
                    SpanKind::ToolTurn { turn },
                    SpanStatus::Ok,
                    None,
                ));
            }
        }
    }
}
