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

        // Non-JSON response: avoid false-negative "low risk" from a malformed reply.
        if payload.get("raw").is_some() {
            let preview: String = payload["raw"].as_str().unwrap_or("").chars().take(200).collect();
            return Ok(AgentOutput {
                role: AgentRole::Risk,
                summary: format!("[PARSE_ERROR] Risk agent returned non-JSON: {preview}"),
                payload: serde_json::json!({
                    "risk_level": "unknown",
                    "reason": "Risk agent failed to produce a valid JSON response.",
                    "risk_unavailable": true,
                }),
                success: false,
                tokens,
            });
        }

        let risk_level_raw = payload
            .get("risk_level")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        // Normalise to the allowed set so downstream consumers can rely on a
        // closed enum. An unrecognised value is mapped to "unknown" rather than
        // silently producing a wrong colour / badge in the UI.
        let risk_level = match risk_level_raw {
            "low" | "medium" | "high" => risk_level_raw,
            _ => "unknown",
        };
        let reason = payload
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no reason provided");

        // If the LLM returned an unrecognised risk_level we treat it as
        // a soft failure so callers can detect the degraded state.
        let success = risk_level != "unknown";

        Ok(AgentOutput {
            role: AgentRole::Risk,
            summary: format!("[{}] {}", risk_level.to_uppercase(), reason),
            payload,
            success,
            tokens,
        })
    }
}
