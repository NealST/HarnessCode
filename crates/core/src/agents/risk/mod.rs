//! Risk agent — analyses a code diff for semantic risk and security implications.

pub mod context;

use super::{parse_json_or_wrap, simple_complete, Agent, AgentError, AgentOutput, AgentRole};
use crate::llm::LlmProvider;
use crate::observability::TokenUsage;
use std::sync::Arc;
use tracing::info;

/// Risk agent backed by an LLM.
///
/// Receives the Coder's JSON payload and returns a structured risk assessment
/// (`risk_level`, `reason`, `affected_areas`, `breaking_change`, …).
///
/// Risk output is always informational — it never causes a pipeline retry.
pub struct LlmRiskAgent {
    pub llm: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl Agent for LlmRiskAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Risk
    }

    async fn execute(&self, code_context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Risk, "Analysing code changes for semantic risk");

        let response = simple_complete(
            &self.llm,
            context::SYSTEM,
            context::user_message(code_context),
        )
        .await?;
        let tokens = Some(TokenUsage::from(&response));
        let payload = parse_json_or_wrap(&response.content);

        let risk_level = payload
            .get("risk_level")
            .and_then(|v| v.as_str())
            .unwrap_or("low");
        let reason = payload
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no significant risk detected");

        Ok(AgentOutput {
            role: AgentRole::Risk,
            summary: format!("[{}] {}", risk_level.to_uppercase(), reason),
            payload,
            success: true,
            tokens,
        })
    }
}
