//! Planner agent — decomposes a coding task into an ordered execution plan.
//!
//! The planner uses a **read-only tool loop** to explore the codebase before
//! writing its plan.  It only has access to sensor tools (`read_file`,
//! `list_directory`, `search_files`), so it can never modify anything.

pub mod context;

use super::{parse_json_or_wrap, Agent, AgentError, AgentOutput, AgentRole};
use crate::controller::{guardrails::ExecutionGuard, tool_loop::run_tool_loop};
use crate::llm::{LlmMessage, LlmProvider};
use crate::observability::ObsCtx;
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tracing::info;

/// Planner agent backed by an LLM with read-only tool access.
///
/// Drives a sensor-only `run_tool_loop`:
/// 1. The LLM calls `list_directory`, `search_files`, and `read_file` to
///    understand the real codebase structure.
/// 2. Once it has enough context, it produces a final JSON plan referencing
///    actual file paths.
///
/// The `guard` should be built from [`ToolRegistry::with_sensors`] limits —
/// a lean step budget (≤ 10 turns) and a short timeout are recommended.
pub struct LlmPlannerAgent {
    pub llm: Arc<dyn LlmProvider>,
    pub registry: Arc<ToolRegistry>,
    pub guard: Arc<ExecutionGuard>,
    pub obs: ObsCtx,
}

#[async_trait::async_trait]
impl Agent for LlmPlannerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Planner
    }

    async fn execute(&self, task: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Planner, "Exploring codebase and building execution plan");

        let messages = vec![
            LlmMessage::system(context::SYSTEM),
            LlmMessage::user(context::user_message(task)),
        ];

        let (text, tokens) =
            run_tool_loop(&self.llm, messages, &self.registry, &self.guard, &self.obs, None).await?;
        let payload = parse_json_or_wrap(&text);

        // A valid plan must have a non-empty `steps` array.  If the LLM returned
        // something that couldn't be parsed as JSON (wrapped in `{"raw":"..."}`) or
        // a JSON object without `steps`, mark the output as a failure so the
        // controller's retry logic can kick in.
        let steps = payload
            .get("steps")
            .and_then(|s| s.as_array());
        let has_valid_plan = steps.map(|a| !a.is_empty()).unwrap_or(false);

        let summary = if has_valid_plan {
            format!("Plan ready: {} step(s)", steps.unwrap().len())
        } else {
            "Planner did not produce a valid JSON plan".to_string()
        };

        Ok(AgentOutput {
            role: AgentRole::Planner,
            summary,
            payload,
            success: has_valid_plan,
            tokens: Some(tokens),
        })
    }
}
