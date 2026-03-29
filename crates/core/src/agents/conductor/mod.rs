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

        let (text, tokens) =
            run_tool_loop(&self.llm, messages, &self.registry, &self.guard, &self.obs, self.drift.as_ref()).await?;
        let payload = parse_json_or_wrap(&text);

        // Consider the phase failed when the Conductor explicitly signals
        // success_criteria_met = false.  Missing key defaults to true (old
        // model behaviour) so we don't break compability when the field is absent.
        let success_criteria_met = payload
            .get("success_criteria_met")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let summary = payload
            .get("explanation")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Code changes generated".to_string());

        Ok(AgentOutput {
            role: AgentRole::Conductor,
            summary,
            payload,
            success: success_criteria_met,
            tokens: Some(tokens),
        })
    }
}
