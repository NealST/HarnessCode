//! Planner agent — decomposes a coding task into an ordered execution plan.

pub mod context;

use super::{parse_json_or_wrap, simple_complete, Agent, AgentError, AgentOutput, AgentRole};
use crate::llm::LlmProvider;
use crate::observability::TokenUsage;
use std::sync::Arc;
use tracing::info;

/// Planner agent backed by an LLM.
///
/// Receives a free-text task description and returns a structured JSON plan
/// listing steps, affected files, success criteria, and complexity.
pub struct LlmPlannerAgent {
    pub llm: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl Agent for LlmPlannerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Planner
    }

    async fn execute(&self, task: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Planner, "Analysing task and building execution plan");

        let response = simple_complete(&self.llm, context::SYSTEM, context::user_message(task)).await?;
        let tokens = Some(TokenUsage::from(&response));
        let payload = parse_json_or_wrap(&response.content);

        let summary = payload
            .get("steps")
            .and_then(|s| s.as_array())
            .map(|steps| format!("Plan ready: {} step(s)", steps.len()))
            .unwrap_or_else(|| "Execution plan generated".to_string());

        Ok(AgentOutput {
            role: AgentRole::Planner,
            summary,
            payload,
            success: true,
            tokens,
        })
    }
}
