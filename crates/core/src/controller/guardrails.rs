//! Execution safety guardrails for the agentic tool loop.
//!
//! [`ExecutionGuard`] is the Control layer's "safety net": it enforces hard
//! resource limits that the LLM cannot reason its way past.
//!
//! ## Guardrails
//!
//! | Check | Trigger | Default |
//! |-------|---------|---------|
//! | Step budget | More than `max_tool_turns` tool-use rounds | 100 turns |
//! | Budget warning | 80 % of `max_tool_turns` reached — inject hint | always on |
//! | Duplicate deduplication | Identical (name + canonical args) calls in one batch | always on |

use std::sync::atomic::{AtomicUsize, Ordering};
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
}

/// Result of a successful `increment_turns` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// Budget is healthy — no action required.
    Ok,
    /// The 80 % threshold was just crossed.  The caller should inject a
    /// system-level hint so the LLM starts wrapping up.
    Warning { used: usize, remaining: usize },
}

// ──────────────────────────────────────────────
// Guard
// ──────────────────────────────────────────────

/// Stateful execution guard passed into every tool loop invocation.
///
/// Create one guard per agent execution (not shared across pipeline retries so
/// that each attempt gets a fresh step budget).
pub struct ExecutionGuard {
    // ── configuration ─────────────────────────────────────────────────────
    /// Maximum number of tool-use rounds before forcing termination.
    pub max_tool_turns: usize,

    // ── runtime state ─────────────────────────────────────────────────────
    tool_turns_used: AtomicUsize,
}

impl Default for ExecutionGuard {
    fn default() -> Self {
        Self {
            max_tool_turns: 100,
            tool_turns_used: AtomicUsize::new(0),
        }
    }
}

impl ExecutionGuard {
    /// Create a guard with the given step budget.
    pub fn new(max_tool_turns: usize) -> Self {
        Self {
            max_tool_turns,
            tool_turns_used: AtomicUsize::new(0),
        }
    }

    /// Create a guard with no step budget (unlimited turns).
    ///
    /// Used for the Planner agent, which only has read-only sensor tools.
    pub fn unlimited() -> Self {
        Self {
            max_tool_turns: usize::MAX,
            tool_turns_used: AtomicUsize::new(0),
        }
    }

    // ── Check methods ──────────────────────────────────────────────────────

    /// Increment the turn counter and check the step-budget limit.
    ///
    /// Returns [`StepStatus::Warning`] exactly once — the first turn that
    /// crosses 80 % of `max_tool_turns` — so the caller can inject a
    /// system-level "wrap up" hint into the conversation.
    pub fn increment_turns(&self) -> Result<StepStatus, GuardrailViolation> {
        let used = self.tool_turns_used.fetch_add(1, Ordering::Relaxed) + 1;
        if used > self.max_tool_turns {
            return Err(GuardrailViolation::StepBudgetExceeded {
                used,
                limit: self.max_tool_turns,
            });
        }
        // Fire the warning exactly once when the 80 % line is crossed.
        let threshold = self.warning_threshold();
        if used == threshold {
            Ok(StepStatus::Warning {
                used,
                remaining: self.max_tool_turns - used,
            })
        } else {
            Ok(StepStatus::Ok)
        }
    }

    /// The turn number at which the 80 % warning fires.
    fn warning_threshold(&self) -> usize {
        // For unlimited guards (usize::MAX) this will never match.
        (self.max_tool_turns as f64 * 0.8).ceil() as usize
    }

    /// Deduplicate a batch of tool calls.
    ///
    /// Removes calls whose (name, canonical-args) pair already appeared in
    /// the same batch (LLM hallucination deduplication).
    ///
    /// Returns the cleaned batch.  Never errors — dedup is silent.
    pub fn sanitise_calls(&self, calls: Vec<ToolCall>) -> Vec<ToolCall> {
        let mut seen: Vec<String> = Vec::new();
        calls
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
            .collect()
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
        let guard = ExecutionGuard::new(20);
        let calls: Vec<ToolCall> = (0..10)
            .map(|i| make_call("read_file", serde_json::json!({"path": format!("file{i}.rs")})))
            .collect();
        let cleaned = guard.sanitise_calls(calls);
        // No concurrent ceiling — all 10 unique calls pass through.
        assert_eq!(cleaned.len(), 10);
    }

    #[test]
    fn step_budget_exceeded() {
        let guard = ExecutionGuard::new(2);
        guard.increment_turns().unwrap();
        guard.increment_turns().unwrap();
        assert!(guard.increment_turns().is_err());
    }

    #[test]
    fn warning_fires_at_80_percent() {
        let guard = ExecutionGuard::new(10);
        // Turns 1-7 should be Ok.
        for _ in 0..7 {
            assert_eq!(guard.increment_turns().unwrap(), StepStatus::Ok);
        }
        // Turn 8 (80 %) should trigger the warning.
        assert!(matches!(guard.increment_turns().unwrap(), StepStatus::Warning { used: 8, remaining: 2 }));
        // Turn 9-10 should be Ok (warning only fires once).
        assert_eq!(guard.increment_turns().unwrap(), StepStatus::Ok);
        assert_eq!(guard.increment_turns().unwrap(), StepStatus::Ok);
        // Turn 11 should exceed the budget.
        assert!(guard.increment_turns().is_err());
    }
}
