//! Reviewer agent — validates correctness and decides pass/fail.

pub mod context;

use super::{parse_json_or_wrap, simple_complete, Agent, AgentError, AgentOutput, AgentRole};
use crate::llm::LlmProvider;
use crate::observability::TokenUsage;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::info;

/// Reviewer agent backed by an LLM.
///
/// Receives the combined plan + code-changes + risk-assessment context and returns a
/// structured review verdict (`approved`, `criteria_met`, `issues`, `security_concerns`, …).
/// The pipeline only converges when **both** `approved` and `criteria_met` are true,
/// closing the TOTE (Test-Operate-Test-Exit) loop.
pub struct LlmReviewerAgent {
    pub llm: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl Agent for LlmReviewerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Reviewer
    }

    async fn execute(&self, review_context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Reviewer, "Reviewing generated code changes");

        let response = simple_complete(
            &self.llm,
            context::SYSTEM,
            context::user_message(review_context),
        )
        .await?;
        let tokens = Some(TokenUsage::from(&response));
        let mut payload = parse_json_or_wrap(&response.content);

        // Rust-side drift check: if the Coder touched files not in the plan, append
        // a note to `issues` so the user is aware. This is advisory — it does not
        // override the LLM's verdict or force a pipeline retry.
        if let Some(issue) = detect_unexplained_file_scope_drift(review_context) {
            append_drift_issue(&mut payload, &issue);
        }

        // Read the LLM's advisory verdict for display purposes.
        // Defaults to false if missing (malformed JSON) — conservative but only
        // affects what the user sees, not whether the pipeline continues.
        let approved = payload
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let criteria_met = payload
            .get("criteria_met")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let recommendation = payload
            .get("recommendation")
            .and_then(|r| r.as_str())
            .unwrap_or(if approved && criteria_met {
                "Review complete — no issues found"
            } else if !criteria_met {
                "Review complete — success criteria not fully met"
            } else {
                "Review complete — issues identified, see details"
            });

        // The reviewer always succeeds at producing a review. Whether the changes
        // are accepted is a decision for the user, not the pipeline.
        Ok(AgentOutput {
            role: AgentRole::Reviewer,
            summary: recommendation.to_string(),
            payload,
            success: true,
            tokens,
        })
    }
}

fn detect_unexplained_file_scope_drift(review_context: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(review_context).ok()?;
    let plan = parsed.get("plan")?;
    let code_changes = parsed.get("code_changes")?;

    // Aggregate planned files from both flat (`plan.affected_files`) and phased
    // (`plan.phases[*].affected_files`) formats.
    let planned: BTreeSet<String> = {
        let flat = value_array_strings(plan, "affected_files");
        if !flat.is_empty() {
            flat
        } else {
            plan.get("phases")
                .and_then(|p| p.as_array())
                .map(|phases| {
                    phases
                        .iter()
                        .flat_map(|ph| value_array_strings(ph, "affected_files"))
                        .collect()
                })
                .unwrap_or_default()
        }
    };

    // Primary source: actual_changes[].path (ground-truth from conductor).
    // Fallback 1: affected_files list injected by conductor.
    // Fallback 2: classic unified-diff header parsing.
    let actual: BTreeSet<String> = extract_changed_files_from_code_changes(code_changes);

    if planned.is_empty() || actual.is_empty() {
        return None;
    }

    // Only flag extras (files touched outside the plan). Missing files are not a
    // problem — the Coder may have found a more targeted approach than the plan predicted.
    let extras: Vec<String> = actual.difference(&planned).cloned().collect();

    if extras.is_empty() {
        return None;
    }

    Some(format!(
        "Coder modified files not listed in plan.affected_files: [{}]. \
         Verify these changes are intentional.",
        extras.join(", ")
    ))
}

fn extract_changed_files_from_code_changes(code_changes: &Value) -> BTreeSet<String> {
    // Priority 1: actual_changes[].path — ground-truth written by conductor.
    if let Some(actual_changes) = code_changes.get("actual_changes").and_then(|v| v.as_array()) {
        let paths: BTreeSet<String> = actual_changes
            .iter()
            .filter_map(|entry| entry.get("path").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }

    // Priority 2: affected_files — deduped list injected by conductor.
    let affected = value_array_strings(code_changes, "affected_files");
    if !affected.is_empty() {
        return affected;
    }

    // Priority 3: unified-diff header parsing (legacy / fallback).
    // Only collect `+++ b/` (new path) lines. `--- a/` (old path) is skipped
    // to avoid false drift alerts for renamed files — a rename produces
    // both paths but only the new name is relevant to the plan.
    code_changes
        .get("diff")
        .and_then(|v| v.as_str())
        .map(|diff| {
            diff.lines()
                .filter_map(|line| {
                    line.strip_prefix("+++ b/")
                        .filter(|&p| p != "/dev/null")
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn value_array_strings(value: &Value, key: &str) -> BTreeSet<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Append a drift note to the `issues` array in the LLM payload.
/// Skips the append if an identical message is already present (prevents
/// duplicating an issue that the LLM itself may have already reported).
/// Does not modify `approved`, `criteria_met`, or `recommendation` —
/// those reflect the LLM's assessment and are advisory for the user.
fn append_drift_issue(payload: &mut Value, issue: &str) {
    if !payload.is_object() {
        return; // Non-JSON response; leave as-is.
    }
    if let Some(obj) = payload.as_object_mut() {
        let issues_value = obj
            .entry("issues".to_string())
            .or_insert_with(|| Value::Array(vec![]));
        match issues_value {
            Value::Array(arr) => {
                // Deduplicate: don't append if LLM already recorded the same files.
                // Use char-based prefix to avoid panicking on multi-byte UTF-8 (e.g. CJK paths).
                let prefix: String = issue.chars().take(40).collect();
                let already_present = arr.iter().any(|v| {
                    v.as_str().is_some_and(|s| s.contains(prefix.as_str()))
                });
                if !already_present {
                    arr.push(Value::String(issue.to_string()));
                }
            }
            _ => *issues_value = Value::Array(vec![Value::String(issue.to_string())]),
        }
    }
}
