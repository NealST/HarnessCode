//! Conductor agent — executes the plan by reading/writing files via the tool loop.

pub mod context;

use super::{parse_json_or_wrap, Agent, AgentError, AgentOutput, AgentRole};
use super::drift_judge::DriftParams;
use crate::controller::{guardrails::ExecutionGuard, tool_loop::run_tool_loop};
use crate::llm::{LlmMessage, LlmProvider};
use crate::observability::ObsCtx;
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tracing::info;

/// Conductor agent backed by an LLM with full tool access.
///
/// Drives the agentic `run_tool_loop` — the LLM reasons, calls file/shell tools,
/// reads results, and repeats until it produces a final JSON diff summary.
///
/// The `guard` parameter enforces execution safety limits (step budget, timeout,
/// concurrent tool ceiling, per-tool rate limits).
///
/// The `obs` parameter receives ToolTurn and ToolCall spans for this execution.
pub struct LlmConductorAgent {
    pub llm: Arc<dyn LlmProvider>,
    pub registry: Arc<ToolRegistry>,
    pub guard: Arc<ExecutionGuard>,
    pub obs: ObsCtx,
    /// Optional drift-detection configuration.  `None` disables drift checking.
    pub drift: Option<DriftParams>,
}

#[async_trait::async_trait]
impl Agent for LlmConductorAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Conductor
    }

    async fn execute(&self, plan_context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Conductor, "Executing phase");

        let messages = vec![
            LlmMessage::system(context::SYSTEM),
            LlmMessage::user(context::user_message(plan_context)),
        ];

        let (text, tokens, written_files) =
            run_tool_loop(&self.llm, messages, &self.registry, &self.guard, &self.obs, self.drift.as_ref()).await?;
        let mut payload = parse_json_or_wrap(&text);

        // If the LLM returned free text instead of JSON, parse_json_or_wrap wraps
        // it as {"raw": "..."}. Treat that as a failure rather than silently
        // reporting success with an empty diff.
        let non_json_response = payload.get("raw").is_some();

        // Consider the phase failed when the Conductor explicitly signals
        // success_criteria_met = false.  Missing key defaults to true (old
        // model behaviour) so we don't break compatibility when the field is absent.
        let success_criteria_met = if non_json_response {
            false
        } else {
            payload
                .get("success_criteria_met")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        };

        let summary = if non_json_response {
            let preview: String = payload["raw"].as_str().unwrap_or("").chars().take(200).collect();
            format!("Conductor returned non-JSON response: {preview}")
        } else {
            payload
                .get("explanation")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Code changes generated".to_string())
        };

        // Inject ground-truth tracking data collected from tool calls.
        // `affected_files` is the deduplicated sorted path list.
        // `actual_changes` preserves full history (including multiple edits to the
        // same file) so Risk can analyse the real content / diffs rather than the
        // LLM-generated diff summary.
        if !non_json_response {
            let affected: Vec<String> = written_files
                .iter()
                .map(|wf| wf.path.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(str::to_string)
                .collect();
            let files_changed = affected.len();
            payload["affected_files"] = serde_json::json!(affected);
            payload["files_changed"] = serde_json::json!(files_changed);
            payload["actual_changes"] = serde_json::json!(written_files);
        }

        Ok(AgentOutput {
            role: AgentRole::Conductor,
            summary,
            payload,
            success: success_criteria_met,
            tokens: Some(tokens),
        })
    }
}
