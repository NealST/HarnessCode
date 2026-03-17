//! Coder agent — implements the plan by reading/writing files via the tool loop.

pub mod context;

use super::{parse_json_or_wrap, Agent, AgentError, AgentOutput, AgentRole};
use crate::controller::{guardrails::ExecutionGuard, tool_loop::run_tool_loop};
use crate::llm::{LlmMessage, LlmProvider};
use crate::observability::ObsCtx;
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tracing::info;

/// Coder agent backed by an LLM with full tool access.
///
/// Drives the agentic `run_tool_loop` — the LLM reasons, calls file/shell tools,
/// reads results, and repeats until it produces a final JSON diff summary.
///
/// The `guard` parameter enforces execution safety limits (step budget, timeout,
/// concurrent tool ceiling, per-tool rate limits).
///
/// The `obs` parameter receives ToolTurn and ToolCall spans for this execution.
pub struct LlmCoderAgent {
    pub llm: Arc<dyn LlmProvider>,
    pub registry: Arc<ToolRegistry>,
    pub guard: Arc<ExecutionGuard>,
    pub obs: ObsCtx,
}

#[async_trait::async_trait]
impl Agent for LlmCoderAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Coder
    }

    async fn execute(&self, plan_context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Coder, "Generating code changes from plan");

        let messages = vec![
            LlmMessage::system(context::SYSTEM),
            LlmMessage::user(context::user_message(plan_context)),
        ];

        let (text, tokens) =
            run_tool_loop(&self.llm, messages, &self.registry, &self.guard, &self.obs).await?;
        let payload = parse_json_or_wrap(&text);

        let summary = payload
            .get("explanation")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Code changes generated".to_string());

        Ok(AgentOutput {
            role: AgentRole::Coder,
            summary,
            payload,
            success: true,
            tokens: Some(tokens),
        })
    }
}
