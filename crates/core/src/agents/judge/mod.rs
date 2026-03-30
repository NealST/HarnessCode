//! Judge agent — decides whether the request is ready for planning, needs scoping,
//! or requires user clarification.

pub mod context;

use super::{parse_json_or_wrap, simple_complete, AgentError};
use crate::llm::LlmProvider;
use std::sync::Arc;

/// Independent request-completeness judge.
pub struct JudgeAgent {
    pub llm: Arc<dyn LlmProvider>,
}

impl JudgeAgent {
    /// Produce a structured routing decision for the current request context.
    pub async fn judge(&self, request_context_json: &str) -> Result<JudgeDecision, AgentError> {
        let response = simple_complete(&self.llm, context::SYSTEM, request_context_json).await?;
        let payload = parse_json_or_wrap(&response.content);
        let route = payload
            .get("route")
            .and_then(|value| value.as_str())
            .unwrap_or("scoper")
            .to_string();
        Ok(JudgeDecision {
            route: route.clone(),
            route_reason_code: payload
                .get("route_reason_code")
                .and_then(|value| value.as_str())
                .unwrap_or("needs_repository_grounding")
                .to_string(),
            ready_for_scoper: route == "scoper",
            ready_for_planner: route == "planner",
            // LOGIC-6: if route=clarify, always ask for clarification — even if the LLM
            // returned an inconsistent explicit false for ask_user_clarification.
            ask_user_clarification: if route == "clarify" {
                true
            } else {
                payload
                    .get("ask_user_clarification")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
            },
            // BUG-5: fall back to current_request (not the entire RequestContext JSON) when
            // the LLM's effective_request field is missing or blank.
            effective_request: payload
                .get("effective_request")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    serde_json::from_str::<serde_json::Value>(request_context_json)
                        .ok()
                        .and_then(|v| v.get("current_request").and_then(|v| v.as_str()).map(str::to_string))
                        .unwrap_or_default()
                }),
            decision_factors: JudgeDecisionFactors {
                goal_is_concrete: payload
                    .get("decision_factors")
                    .and_then(|value| value.get("goal_is_concrete"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                constraints_are_stable: payload
                    .get("decision_factors")
                    .and_then(|value| value.get("constraints_are_stable"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                history_resolves_references: payload
                    .get("decision_factors")
                    .and_then(|value| value.get("history_resolves_references"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                repository_grounding_needed: payload
                    .get("decision_factors")
                    .and_then(|value| value.get("repository_grounding_needed"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(route == "scoper"),
                prior_scope_can_be_reused: payload
                    .get("decision_factors")
                    .and_then(|value| value.get("prior_scope_can_be_reused"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            },
            skip_scoper_criteria_met: payload_strings(&payload, "skip_scoper_criteria_met"),
            missing_information: payload_strings(&payload, "missing_information"),
            clarifying_questions: payload_strings(&payload, "clarifying_questions"),
            confidence: payload
                .get("confidence")
                .and_then(|value| value.as_str())
                .unwrap_or("medium")
                .to_string(),
            raw: payload,
            tokens: response.prompt_tokens.zip(response.completion_tokens).map(|(prompt, completion)| {
                crate::observability::TokenUsage::new(prompt, completion)
            }),
        })
    }
}

/// Structured output of the Judge agent.
#[derive(Debug, Clone)]
pub struct JudgeDecision {
    pub route: String,
    pub route_reason_code: String,
    pub ready_for_scoper: bool,
    pub ready_for_planner: bool,
    pub ask_user_clarification: bool,
    pub effective_request: String,
    pub decision_factors: JudgeDecisionFactors,
    pub skip_scoper_criteria_met: Vec<String>,
    pub missing_information: Vec<String>,
    pub clarifying_questions: Vec<String>,
    pub confidence: String,
    pub raw: serde_json::Value,
    pub tokens: Option<crate::observability::TokenUsage>,
}

#[derive(Debug, Clone)]
pub struct JudgeDecisionFactors {
    pub goal_is_concrete: bool,
    pub constraints_are_stable: bool,
    pub history_resolves_references: bool,
    pub repository_grounding_needed: bool,
    pub prior_scope_can_be_reused: bool,
}

fn payload_strings(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChunkStream, LlmCompletion, LlmError, LlmMessage, LlmProvider, LlmResponse, ToolDef};

    struct MockLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockLlm {
        fn provider_name(&self) -> &str { "mock" }
        fn model_name(&self) -> &str { "mock" }

        async fn complete(&self, _messages: &[LlmMessage]) -> Result<LlmResponse, LlmError> {
            Ok(LlmResponse {
                content: self.response.clone(),
                model: "mock".into(),
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
            })
        }

        async fn stream(&self, _messages: &[LlmMessage]) -> Result<ChunkStream, LlmError> {
            unimplemented!()
        }

        async fn complete_with_tools(
            &self,
            _messages: &[LlmMessage],
            _tools: &[ToolDef],
        ) -> Result<LlmCompletion, LlmError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn judge_parses_structured_decision() {
        let judge = JudgeAgent {
            llm: Arc::new(MockLlm {
                response: r#"{"route":"planner","route_reason_code":"sufficient_prior_scope","ask_user_clarification":false,"effective_request":"Apply the already approved event wiring change","decision_factors":{"goal_is_concrete":true,"constraints_are_stable":true,"history_resolves_references":true,"repository_grounding_needed":false,"prior_scope_can_be_reused":true},"skip_scoper_criteria_met":["objective_clear","constraints_clear","prior_scope_reusable"],"missing_information":[],"clarifying_questions":[],"confidence":"high"}"#.into(),
            }),
        };

        let decision = judge.judge("{}").await.unwrap();
        assert!(decision.ready_for_planner);
        assert!(!decision.ready_for_scoper);
        assert_eq!(decision.route_reason_code, "sufficient_prior_scope");
        assert!(decision.decision_factors.prior_scope_can_be_reused);
        assert_eq!(decision.skip_scoper_criteria_met.len(), 3);
        assert_eq!(decision.effective_request, "Apply the already approved event wiring change");
    }
}