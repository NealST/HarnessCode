//! Reviewer agent — validates correctness and decides pass/fail.

pub mod context;

use super::{parse_json_or_wrap, simple_complete, Agent, AgentError, AgentOutput, AgentRole};
use crate::llm::LlmProvider;
use crate::observability::TokenUsage;
use serde_json::{json, Value};
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
        let consistency_issue = detect_unexplained_file_scope_drift(review_context);

        if let Some(issue) = consistency_issue.as_ref() {
            enforce_rejection_with_issue(&mut payload, issue);
        }

        // Fail-safe defaults: if the LLM returns malformed JSON without these
        // keys, we conservatively treat the review as rejected so the pipeline
        // retries rather than silently approving broken output.
        let approved = payload
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let criteria_met = payload
            .get("criteria_met")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let passed = approved && criteria_met;
        let recommendation = payload
            .get("recommendation")
            .and_then(|r| r.as_str())
            .unwrap_or(if passed {
                "Approved"
            } else if !criteria_met {
                "Rejected — success criteria not met"
            } else {
                "Rejected — revisions required"
            });

        Ok(AgentOutput {
            role: AgentRole::Reviewer,
            summary: recommendation.to_string(),
            payload,
            success: passed,
            tokens,
        })
    }
}

fn detect_unexplained_file_scope_drift(review_context: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(review_context).ok()?;
    let plan = parsed.get("plan")?;
    let code_changes = parsed.get("code_changes")?;

    // Handle both old flat format (`plan.affected_files`) and new phased format
    // (`plan.phases[*].affected_files`).  Aggregate all phase-level candidate files
    // into a single set so the drift check works regardless of plan shape.
    let planned: BTreeSet<String> = {
        let flat = value_array_strings(plan, "affected_files");
        if !flat.is_empty() {
            flat.into_iter().collect()
        } else if let Some(phases) = plan.get("phases").and_then(|p| p.as_array()) {
            phases
                .iter()
                .flat_map(|ph| value_array_strings(ph, "affected_files"))
                .collect()
        } else {
            BTreeSet::new()
        }
    };
    let actual: BTreeSet<String> = extract_changed_files_from_code_changes(code_changes);

    if planned.is_empty() || actual.is_empty() {
        return None;
    }

    let extras: Vec<String> = actual.difference(&planned).cloned().collect();
    let missing: Vec<String> = planned.difference(&actual).cloned().collect();

    if extras.is_empty() && missing.is_empty() {
        return None;
    }

    let delta_reason = code_changes
        .get("affected_files_delta_reason")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if !delta_reason.is_empty() {
        return None;
    }

    let extras_text = if extras.is_empty() {
        "none".to_string()
    } else {
        extras.join(", ")
    };
    let missing_text = if missing.is_empty() {
        "none".to_string()
    } else {
        missing.join(", ")
    };

    Some(format!(
        "Changed files drifted from plan.affected_files without explanation (affected_files_delta_reason is empty). extras: [{extras_text}]; missing: [{missing_text}]"
    ))
}

fn extract_changed_files_from_code_changes(code_changes: &Value) -> BTreeSet<String> {
    let mut files = BTreeSet::new();

    if let Some(diff) = code_changes.get("diff").and_then(|v| v.as_str()) {
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ b/") {
                if path != "/dev/null" {
                    files.insert(path.to_string());
                }
            } else if let Some(path) = line.strip_prefix("--- a/") {
                if path != "/dev/null" {
                    files.insert(path.to_string());
                }
            }
        }
    }

    files
}

fn value_array_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn enforce_rejection_with_issue(payload: &mut Value, issue: &str) {
    if !payload.is_object() {
        *payload = json!({
            "approved": false,
            "criteria_met": false,
            "issues": [issue],
            "security_concerns": [],
            "recommendation": "Rejected — changed file set drifted from plan without explanation",
        });
        return;
    }

    if let Some(obj) = payload.as_object_mut() {
        obj.insert("approved".to_string(), Value::Bool(false));
        obj.insert("criteria_met".to_string(), Value::Bool(false));

        let issues_value = obj
            .entry("issues".to_string())
            .or_insert_with(|| Value::Array(vec![]));
        match issues_value {
            Value::Array(arr) => arr.push(Value::String(issue.to_string())),
            _ => {
                *issues_value = Value::Array(vec![Value::String(issue.to_string())]);
            }
        }

        obj.entry("security_concerns".to_string())
            .or_insert_with(|| Value::Array(vec![]));
        obj.insert(
            "recommendation".to_string(),
            Value::String("Rejected — changed file set drifted from plan without explanation".to_string()),
        );
    }
}
