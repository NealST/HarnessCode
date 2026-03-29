//! Scoper agent — frames the user's request before planning starts.

pub mod context;

use super::{parse_json_or_wrap, Agent, AgentError, AgentOutput, AgentRole};
use crate::controller::{guardrails::ExecutionGuard, tool_loop::run_tool_loop};
use crate::llm::{LlmMessage, LlmProvider};
use crate::observability::ObsCtx;
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tracing::info;

/// Scoper agent backed by an LLM with read-only tool access.
pub struct LlmScoperAgent {
    pub llm: Arc<dyn LlmProvider>,
    pub registry: Arc<ToolRegistry>,
    pub guard: Arc<ExecutionGuard>,
    pub obs: ObsCtx,
}

#[async_trait::async_trait]
impl Agent for LlmScoperAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Scoper
    }

    async fn execute(&self, task: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Scoper, "Framing user request and defining execution boundaries");

        let messages = vec![
            LlmMessage::system(context::SYSTEM),
            LlmMessage::user(context::user_message(task)),
        ];

        let (text, tokens, _) =
            run_tool_loop(&self.llm, messages, &self.registry, &self.guard, &self.obs, None).await?;
        let payload = parse_json_or_wrap(&text);

        let objective = payload
            .get("objective")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let success_criteria = payload
            .get("success_criteria")
            .and_then(|v| v.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);

        let is_valid = objective.is_some() && success_criteria;
        let summary = if let Some(objective) = objective {
            format!("Problem framed: {objective}")
        } else {
            "Scoper did not produce a valid problem frame".to_string()
        };

        Ok(AgentOutput {
            role: AgentRole::Scoper,
            summary,
            payload,
            success: is_valid,
            tokens: Some(tokens),
        })
    }
}