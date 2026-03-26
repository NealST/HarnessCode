//! Reviewer agent — validates correctness and decides pass/fail.

pub mod context;

use super::{parse_json_or_wrap, simple_complete, Agent, AgentError, AgentOutput, AgentRole};
use crate::llm::LlmProvider;
use crate::observability::TokenUsage;
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
        let payload = parse_json_or_wrap(&response.content);

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
