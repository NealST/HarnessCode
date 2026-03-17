//! Execution safety guardrails for the agentic tool loop.
//!
//! [`ExecutionGuard`] is the Control layer's "safety net": it enforces hard
//! resource limits that the LLM cannot reason its way past.
//!
//! ## Guardrails
//!
//! | Check | Trigger | Default |
//! |-------|---------|---------|
//! | Step budget | More than `max_tool_turns` tool-use rounds | 20 turns |
//! | Timeout | Wall-clock elapsed > `pipeline_timeout` | 300 s |
//! | Concurrent tool ceiling | Single `NeedTools` batch > `max_concurrent_tools` | 8 |
//! | Per-tool rate limit | One tool called more than `per_tool_limit[name]` times | 10× run_command |
//! | Duplicate deduplication | Identical (name + canonical args) calls in one batch | always on |

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use thiserror::Error;

use crate::llm::ToolCall;

// ──────────────────────────────────────────────
// Violation type
// ──────────────────────────────────────────────

/// A hard safety limit was exceeded during agentic execution.
#[derive(Debug, Clone, Error)]
pub enum GuardrailViolation {
    #[error("step budget exceeded: tool loop ran {used} turns, limit is {limit}")]
    StepBudgetExceeded { used: usize, limit: usize },

    #[error("pipeline timeout: elapsed {elapsed:?}, deadline was {limit:?}")]
    Timeout { elapsed: Duration, limit: Duration },

    #[error("tool rate limit exceeded: '{tool}' called {used} times, limit is {limit}")]
    ToolRateLimitExceeded { tool: String, used: usize, limit: usize },
}

// ──────────────────────────────────────────────
// Guard
// ──────────────────────────────────────────────

/// Stateful execution guard passed into every tool loop invocation.
///
/// Create one guard per Coder execution (not shared across pipeline retries so
/// that each attempt gets a fresh budget).
pub struct ExecutionGuard {
    // ── configuration ─────────────────────────────────────────────────────
    /// Maximum number of tool-use rounds before forcing termination.
    pub max_tool_turns: usize,
    /// Hard wall-clock limit for the entire tool loop.
    pub pipeline_timeout: Option<Duration>,
    /// Maximum tools that may be dispatched in a single `NeedTools` batch.
    pub max_concurrent_tools: usize,
    /// Per-tool-name invocation ceiling for the entire guard lifetime.
    /// Tools not listed here have no per-tool limit.
    pub per_tool_limit: HashMap<&'static str, usize>,

    // ── runtime state ─────────────────────────────────────────────────────
    tool_turns_used: AtomicUsize,
    tool_call_counts: Mutex<HashMap<String, usize>>,
    started_at: Instant,
}

impl Default for ExecutionGuard {
    fn default() -> Self {
        let mut per_tool_limit = HashMap::new();
        // Actuator tools get tighter rate limits than sensors by default.
        per_tool_limit.insert("run_command", 10);
        per_tool_limit.insert("write_file", 50);
        per_tool_limit.insert("apply_diff", 50);

        Self {
            max_tool_turns: 20,
            pipeline_timeout: Some(Duration::from_secs(300)),
            max_concurrent_tools: 8,
            per_tool_limit,
            tool_turns_used: AtomicUsize::new(0),
            tool_call_counts: Mutex::new(HashMap::new()),
            started_at: Instant::now(),
        }
    }
}

impl ExecutionGuard {
    /// Create a guard with custom settings.
    pub fn new(
        max_tool_turns: usize,
        pipeline_timeout: Option<Duration>,
        max_concurrent_tools: usize,
        per_tool_limit: HashMap<&'static str, usize>,
    ) -> Self {
        Self {
            max_tool_turns,
            pipeline_timeout,
            max_concurrent_tools,
            per_tool_limit,
            tool_turns_used: AtomicUsize::new(0),
            tool_call_counts: Mutex::new(HashMap::new()),
            started_at: Instant::now(),
        }
    }

    // ── Check methods ──────────────────────────────────────────────────────

    /// Check the wall-clock timeout.  Call at the top of every tool loop iteration.
    pub fn check_timeout(&self) -> Result<(), GuardrailViolation> {
        if let Some(limit) = self.pipeline_timeout {
            let elapsed = self.started_at.elapsed();
            if elapsed > limit {
                return Err(GuardrailViolation::Timeout { elapsed, limit });
            }
        }
        Ok(())
    }

    /// Increment the turn counter and check the step-budget limit.
    pub fn increment_turns(&self) -> Result<(), GuardrailViolation> {
        let used = self.tool_turns_used.fetch_add(1, Ordering::Relaxed) + 1;
        if used > self.max_tool_turns {
            return Err(GuardrailViolation::StepBudgetExceeded {
                used,
                limit: self.max_tool_turns,
            });
        }
        Ok(())
    }

    /// Deduplicate and clamp a batch of tool calls.
    ///
    /// Steps performed in order:
    /// 1. Remove calls whose (name, canonical-args) pair already appeared in
    ///    this batch (LLM hallucination deduplication).
    /// 2. Truncate to `max_concurrent_tools` (tool-explosion prevention).
    ///
    /// Returns the cleaned batch.  Never errors — clamping is silent.
    pub fn sanitise_calls(&self, calls: Vec<ToolCall>) -> Vec<ToolCall> {
        // Step 1 — deduplicate within this batch.
        let mut seen: Vec<String> = Vec::new();
        let deduped: Vec<ToolCall> = calls
            .into_iter()
            .filter(|c| {
                let key = dedup_key(&c.name, &c.arguments);
                if seen.contains(&key) {
                    false
                } else {
                    seen.push(key);
                    true
                }
            })
            .collect();

        // Step 2 — enforce concurrent ceiling.
        if deduped.len() > self.max_concurrent_tools {
            deduped.into_iter().take(self.max_concurrent_tools).collect()
        } else {
            deduped
        }
    }

    /// Check and record a per-tool invocation, enforcing rate limits.
    ///
    /// Returns `Err` if the tool has hit its ceiling.  Call after `sanitise_calls`
    /// so duplicate calls have already been removed.
    pub fn check_and_record_tool(&self, tool_name: &str) -> Result<(), GuardrailViolation> {
        let mut counts = self.tool_call_counts.lock().unwrap();
        let count = counts.entry(tool_name.to_string()).or_insert(0);
        *count += 1;

        if let Some(&limit) = self.per_tool_limit.get(tool_name) {
            if *count > limit {
                return Err(GuardrailViolation::ToolRateLimitExceeded {
                    tool: tool_name.to_string(),
                    used: *count,
                    limit,
                });
            }
        }
        Ok(())
    }

    /// Remaining time before the pipeline timeout fires (for retry budget sizing).
    pub fn remaining_timeout(&self) -> Option<Duration> {
        self.pipeline_timeout.map(|limit| {
            limit.saturating_sub(self.started_at.elapsed())
        })
    }
}

// ──────────────────────────────────────────────
// Canonical JSON deduplication key
// ──────────────────────────────────────────────

/// Build a deduplication key from a tool name and its arguments.
///
/// Uses canonical JSON serialisation (object keys sorted alphabetically at
/// every level) so that `{"b":1,"a":2}` and `{"a":2,"b":1}` produce the
/// same key.
fn dedup_key(name: &str, args: &serde_json::Value) -> String {
    format!("{}:{}", name, canonical_json(args))
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<(&str, &serde_json::Value)> = map.iter().map(|(k, v)| (k.as_str(), v)).collect();
            pairs.sort_by_key(|(k, _)| *k);
            let inner: Vec<String> = pairs
                .into_iter()
                .map(|(k, v)| format!("\"{}\":{}", k, canonical_json(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall { id: uuid(), name: name.to_string(), arguments: args }
    }

    fn uuid() -> String {
        // Simple unique ID for tests — not cryptographic.
        format!("id-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos())
    }

    #[test]
    fn dedup_removes_identical_calls() {
        let guard = ExecutionGuard::default();
        let calls = vec![
            make_call("read_file", serde_json::json!({"path": "src/main.rs"})),
            make_call("read_file", serde_json::json!({"path": "src/main.rs"})), // duplicate
            make_call("read_file", serde_json::json!({"path": "src/lib.rs"})),  // different args
        ];
        let cleaned = guard.sanitise_calls(calls);
        assert_eq!(cleaned.len(), 2);
    }

    #[test]
    fn dedup_handles_reordered_args() {
        let guard = ExecutionGuard::default();
        let calls = vec![
            make_call("write_file", serde_json::json!({"path": "a.rs", "content": "x"})),
            make_call("write_file", serde_json::json!({"content": "x", "path": "a.rs"})), // same, different key order
        ];
        let cleaned = guard.sanitise_calls(calls);
        assert_eq!(cleaned.len(), 1);
    }

    #[test]
    fn clamp_enforces_concurrent_ceiling() {
        let mut guard = ExecutionGuard::default();
        guard.max_concurrent_tools = 3;
        let calls: Vec<ToolCall> = (0..10)
            .map(|i| make_call("read_file", serde_json::json!({"path": format!("file{i}.rs")})))
            .collect();
        let cleaned = guard.sanitise_calls(calls);
        assert_eq!(cleaned.len(), 3);
    }

    #[test]
    fn step_budget_exceeded() {
        let guard = ExecutionGuard::new(2, None, 8, HashMap::new());
        guard.increment_turns().unwrap();
        guard.increment_turns().unwrap();
        assert!(guard.increment_turns().is_err());
    }

    #[test]
    fn tool_rate_limit() {
        let mut limits = HashMap::new();
        limits.insert("run_command", 2usize);
        let guard = ExecutionGuard::new(100, None, 8, limits);
        guard.check_and_record_tool("run_command").unwrap();
        guard.check_and_record_tool("run_command").unwrap();
        assert!(guard.check_and_record_tool("run_command").is_err());
    }
}
