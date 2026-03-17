//! The cybernetic Controller — schedules agents and enforces pipeline-level rules.
//!
//! The Controller is the "safety harness" around the multi-agent pipeline:
//! * Drives the Planner → Coder → Risk → Reviewer sequence.
//! * Enforces retry limits and timeout budgets.
//! * Constructs a fresh [`ExecutionGuard`] for each attempt so resource budgets
//!   reset cleanly between retries.
//! * Handles [`GuardrailViolation`] failures with per-type strategies.

use super::events::PipelineEvent;
use super::guardrails::{ExecutionGuard, GuardrailViolation};
use crate::agents::{
    Agent, AgentError, AgentOutput, AgentRole, LlmCoderAgent, LlmPlannerAgent, LlmReviewerAgent,
    LlmRiskAgent,
};
use crate::llm::LlmProvider;
use crate::observability::{
    NoopSink, ObsCtx, SpanKind, SpanSink, SpanStatus, SpanTimer, TokenUsage,
};
use crate::tools::ToolRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Default timeout given to a *retry* attempt: 50% of the original limit.
const RETRY_TIMEOUT_FACTOR: f64 = 0.5;

// ──────────────────────────────────────────────
// Controller
// ──────────────────────────────────────────────

/// The cybernetic controller that drives the multi-agent pipeline.
///
/// Implements a **TOTE** (Test-Operate-Test-Exit) loop:
/// 1. **Test** — Planner evaluates the current state vs. the desired goal.
/// 2. **Operate** — Coder applies changes (with tool calls + guardrails).
/// 3. **Test** — Reviewer checks the result against success criteria.
/// 4. **Exit** — Loop terminates on success or after `max_retries` failures.
pub struct Controller {
    /// Maximum number of full pipeline retries before giving up.
    pub max_retries: usize,
    /// The LLM backend shared by all agents in the pipeline.
    pub llm: Arc<dyn LlmProvider>,
    /// The tool registry available to the Coder during tool-use turns.
    pub registry: Arc<ToolRegistry>,
    /// Guardrail template — cloned into a fresh guard for each pipeline attempt.
    pub guard_template: ExecutionGuardTemplate,
    /// Observability sink — receives spans emitted during pipeline execution.
    /// Defaults to [`NoopSink`]; override with [`Controller::with_obs`].
    pub obs_sink: Arc<dyn SpanSink>,
}

/// Sharable configuration for [`ExecutionGuard`] construction.
#[derive(Clone)]
pub struct ExecutionGuardTemplate {
    pub max_tool_turns: usize,
    pub pipeline_timeout: Option<Duration>,
    pub max_concurrent_tools: usize,
}

impl Default for ExecutionGuardTemplate {
    fn default() -> Self {
        Self {
            max_tool_turns: 20,
            pipeline_timeout: Some(Duration::from_secs(300)),
            max_concurrent_tools: 8,
        }
    }
}

impl ExecutionGuardTemplate {
    /// Per-tool call-count limits shared by all guard builds.
    fn per_tool_limits() -> HashMap<&'static str, usize> {
        let mut m = HashMap::with_capacity(3);
        m.insert("run_command", 10);
        m.insert("write_file", 50);
        m.insert("apply_diff", 50);
        m
    }

    fn build(&self) -> ExecutionGuard {
        ExecutionGuard::new(
            self.max_tool_turns,
            self.pipeline_timeout,
            self.max_concurrent_tools,
            Self::per_tool_limits(),
        )
    }

    /// Build a guard for a retry attempt using a reduced timeout.
    fn build_retry(&self) -> ExecutionGuard {
        let retry_timeout = self.pipeline_timeout.map(|t| {
            Duration::from_secs_f64(t.as_secs_f64() * RETRY_TIMEOUT_FACTOR)
        });
        ExecutionGuard::new(
            self.max_tool_turns,
            retry_timeout,
            self.max_concurrent_tools,
            Self::per_tool_limits(),
        )
    }
}

impl Controller {
    /// Create a [`Controller`] with default guardrail settings and all builtin tools.
    pub fn new(max_retries: usize, llm: Arc<dyn LlmProvider>) -> Self {
        Self {
            max_retries,
            llm,
            registry: Arc::new(ToolRegistry::with_builtins()),
            guard_template: ExecutionGuardTemplate::default(),
            obs_sink: Arc::new(NoopSink),
        }
    }

    /// Create a [`Controller`] with a custom tool registry.
    pub fn new_with_tools(
        max_retries: usize,
        llm: Arc<dyn LlmProvider>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            max_retries,
            llm,
            registry,
            guard_template: ExecutionGuardTemplate::default(),
            obs_sink: Arc::new(NoopSink),
        }
    }

    /// Attach an observability sink.  Returns `self` for builder-style chaining.
    ///
    /// ```rust,ignore
    /// let controller = Controller::new(3, llm)
    ///     .with_obs(Arc::new(TerminalSink::new()));
    /// ```
    pub fn with_obs(mut self, sink: Arc<dyn SpanSink>) -> Self {
        self.obs_sink = sink;
        self
    }

    /// Run the pipeline for `prompt`, returning outputs from all four stages.
    pub async fn run(&self, prompt: &str) -> Result<Vec<AgentOutput>, AgentError> {
        self.run_with_progress(prompt, None).await
    }

    /// Run the pipeline, streaming [`PipelineEvent`]s to `tx` for live UI updates.
    pub async fn run_with_progress(
        &self,
        prompt: &str,
        tx: Option<mpsc::Sender<PipelineEvent>>,
    ) -> Result<Vec<AgentOutput>, AgentError> {
        macro_rules! send {
            ($event:expr) => {
                if let Some(ref tx) = tx {
                    let _ = tx.send($event).await;
                }
            };
        }

        // ── Observability root ────────────────────────────────────────────
        let obs = ObsCtx::new_run(Arc::clone(&self.obs_sink));
        let pipeline_timer = SpanTimer::start();
        let pipeline_span_id = pipeline_timer.id;

        // Track whether we already used our one timeout-retry.
        let mut timeout_retried = false;
        let mut next_retry_is_timeout = false;
        let mut attempt = 0usize;
        let result: Result<Vec<AgentOutput>, AgentError>;

        // Accumulate total tokens across all agents across all attempts.
        let mut pipeline_tokens = TokenUsage::default();

        loop {
            attempt += 1;
            info!(attempt, "Starting pipeline run");

            let use_reduced = next_retry_is_timeout;
            next_retry_is_timeout = false;
            let guard = Arc::new(if use_reduced {
                self.guard_template.build_retry()
            } else {
                self.guard_template.build()
            });

            // Coder's ObsCtx is a child of the pipeline span so its ToolTurn/
            // ToolCall children nest correctly in the span tree.
            let coder_obs = obs.child(pipeline_span_id);

            // Construct agents.
            let planner    = LlmPlannerAgent  { llm: Arc::clone(&self.llm) };
            let coder      = LlmCoderAgent    { llm: Arc::clone(&self.llm), registry: Arc::clone(&self.registry), guard: Arc::clone(&guard), obs: coder_obs };
            let risk_agent = LlmRiskAgent     { llm: Arc::clone(&self.llm) };
            let reviewer   = LlmReviewerAgent { llm: Arc::clone(&self.llm) };

            // ── Stage 1: Planning ────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Planner });
            let stage_timer = SpanTimer::start();
            let plan_result = planner.execute(prompt).await;
            let plan_tokens = plan_result.as_ref().ok().and_then(|o| o.tokens);
            if let Some(t) = plan_tokens { pipeline_tokens.add(t); }
            let plan_span_status = match &plan_result {
                Ok(o) if o.success => SpanStatus::Ok,
                Ok(o) => SpanStatus::Retried { reason: o.summary.clone() },
                Err(e) => SpanStatus::Error { message: e.to_string() },
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Planner, attempt }, plan_span_status, plan_tokens));
            let plan = match plan_result {
                Err(e) => {
                    send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                    result = Err(e);
                    break;
                }
                Ok(o) if !o.success => {
                    warn!(role = %o.role, "Planner reported failure");
                    send!(PipelineEvent::PipelineFailed { error: format!("Planner failed: {}", o.summary) });
                    if attempt >= self.max_retries { result = Err(AgentError::MaxRetriesExceeded(self.max_retries)); break; }
                    continue;
                }
                Ok(o) => o,
            };
            send!(PipelineEvent::StageCompleted { output: plan.clone() });

            // ── Stage 2: Coding with guardrails ──────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Coder });
            let stage_timer = SpanTimer::start();
            let context_for_coder = serde_json::to_string(&plan.payload)?;
            let code_result = coder.execute(&context_for_coder).await;
            let code_tokens = code_result.as_ref().ok().and_then(|o| o.tokens);
            if let Some(t) = code_tokens { pipeline_tokens.add(t); }
            let code_span_status = match &code_result {
                Ok(o) if o.success => SpanStatus::Ok,
                Ok(o) => SpanStatus::Retried { reason: o.summary.clone() },
                // A timeout that will be retried is semantically "retried", not a hard error.
                Err(AgentError::GuardrailViolation(GuardrailViolation::Timeout { .. }))
                    if !timeout_retried && attempt < self.max_retries =>
                    SpanStatus::Retried { reason: "timeout".into() },
                Err(e) => SpanStatus::Error { message: e.to_string() },
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Coder, attempt }, code_span_status, code_tokens));
            let code_output = match code_result {
                Err(AgentError::GuardrailViolation(GuardrailViolation::Timeout { .. })) => {
                    if !timeout_retried && attempt < self.max_retries {
                        timeout_retried = true;
                        next_retry_is_timeout = true;
                        warn!("Coder timed out — retrying with 50% timeout budget");
                        send!(PipelineEvent::PipelineFailed { error: "Coder timed out, retrying with reduced budget".into() });
                        continue;
                    }
                    send!(PipelineEvent::PipelineFailed { error: "Coder timed out (retry exhausted)".into() });
                    result = Err(AgentError::MaxRetriesExceeded(self.max_retries));
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "Coder failed");
                    send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                    if attempt >= self.max_retries { result = Err(AgentError::MaxRetriesExceeded(self.max_retries)); break; }
                    continue;
                }
                Ok(o) if !o.success => {
                    warn!(role = %o.role, "Coder reported failure");
                    send!(PipelineEvent::PipelineFailed { error: format!("Coder failed: {}", o.summary) });
                    if attempt >= self.max_retries { result = Err(AgentError::MaxRetriesExceeded(self.max_retries)); break; }
                    continue;
                }
                Ok(o) => o,
            };
            send!(PipelineEvent::StageCompleted { output: code_output.clone() });

            // ── Stage 3: Risk (informational, never retries) ─────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Risk });
            let stage_timer = SpanTimer::start();
            let risk_context = serde_json::to_string(&code_output.payload)?;
            let risk_result = risk_agent.execute(&risk_context).await;
            let risk_tokens = risk_result.as_ref().ok().and_then(|o| o.tokens);
            if let Some(t) = risk_tokens { pipeline_tokens.add(t); }
            let risk_span_status = match &risk_result {
                Ok(_) => SpanStatus::Ok,
                Err(e) => SpanStatus::Error { message: e.to_string() },
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Risk, attempt }, risk_span_status, risk_tokens));
            let risk_output = risk_result.unwrap_or_else(|e| {
                warn!(error = %e, "Risk agent failed (non-fatal, continuing)");
                AgentOutput {
                    role: AgentRole::Risk,
                    summary: format!("Risk assessment unavailable: {e}"),
                    payload: serde_json::json!({ "risk_level": "unknown", "reason": e.to_string() }),
                    success: true,
                    tokens: None,
                }
            });
            send!(PipelineEvent::StageCompleted { output: risk_output.clone() });

            // ── Stage 4: Review ──────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Reviewer });
            let stage_timer = SpanTimer::start();
            let combined = serde_json::json!({
                "code_changes": code_output.payload,
                "risk_assessment": risk_output.payload,
            });
            let context_for_reviewer = serde_json::to_string(&combined)?;
            let review_result = reviewer.execute(&context_for_reviewer).await;
            let review_tokens = review_result.as_ref().ok().and_then(|o| o.tokens);
            if let Some(t) = review_tokens { pipeline_tokens.add(t); }
            let review_span_status = match &review_result {
                Ok(o) if o.success => SpanStatus::Ok,
                Ok(o) => SpanStatus::Retried { reason: o.summary.clone() },
                Err(e) => SpanStatus::Error { message: e.to_string() },
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Reviewer, attempt }, review_span_status, review_tokens));
            let review = match review_result {
                Err(e) => {
                    send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                    result = Err(e);
                    break;
                }
                Ok(o) if !o.success => {
                    warn!(role = %o.role, "Reviewer rejected; retrying pipeline");
                    send!(PipelineEvent::PipelineFailed { error: format!("Reviewer rejected: {}", o.summary) });
                    if attempt >= self.max_retries { result = Err(AgentError::MaxRetriesExceeded(self.max_retries)); break; }
                    continue;
                }
                Ok(o) => o,
            };
            send!(PipelineEvent::StageCompleted { output: review.clone() });

            info!(attempt, "Pipeline converged successfully");
            result = Ok(vec![plan, code_output, risk_output, review]);
            break;
        }

        // ── Emit root pipeline span and flush ─────────────────────────────
        let pipeline_status = match &result {
            Ok(_) => SpanStatus::Ok,
            Err(e) => SpanStatus::Error { message: e.to_string() },
        };
        obs.record(pipeline_timer.finish(
            obs.run_id,
            None,
            SpanKind::Pipeline { prompt: prompt.to_string() },
            pipeline_status,
            Some(pipeline_tokens),
        ));
        obs.flush();

        result
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{
        ChunkStream, LlmCompletion, LlmError, LlmMessage, LlmProvider, LlmResponse, StreamChunk,
        ToolDef,
    };
    use futures::stream;

    /// Deterministic mock LLM — returns canned JSON matching each agent's expected format.
    struct MockLlmProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockLlmProvider {
        fn provider_name(&self) -> &str { "mock" }
        fn model_name(&self) -> &str { "mock-model" }

        async fn complete(&self, messages: &[LlmMessage]) -> Result<LlmResponse, LlmError> {
            let sys = messages
                .iter()
                .find(|m| m.role == crate::llm::MessageRole::System)
                .map(|m| m.content.as_str())
                .unwrap_or("");

            let content = if sys.contains("planning agent") {
                r#"{"steps":["Analyse codebase","Generate diff","Run tests"],"affected_files":["src/main.rs"],"success_criteria":"tests pass","complexity":"low"}"#
            } else if sys.contains("architect and security engineer") {
                r#"{"risk_level":"low","reason":"no significant risk detected","affected_areas":[],"breaking_change":false,"security_implications":"","cr_focus":"standard code review"}"#
            } else if sys.contains("expert software engineer") {
                r#"{"diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-// TODO\n+// Done","files_changed":1,"explanation":"Replaced placeholder","language":"rust"}"#
            } else {
                r#"{"approved":true,"issues":[],"security_concerns":[],"recommendation":"LGTM"}"#
            };

            Ok(LlmResponse {
                content: content.to_string(),
                model: "mock-model".to_string(),
                prompt_tokens: Some(80),
                completion_tokens: Some(20),
                total_tokens: Some(100),
            })
        }

        async fn stream(&self, _messages: &[LlmMessage]) -> Result<ChunkStream, LlmError> {
            Ok(Box::pin(stream::iter(vec![Ok(StreamChunk {
                delta: "result".to_string(),
                finished: true,
            })])))
        }

        async fn complete_with_tools(
            &self,
            messages: &[LlmMessage],
            _tools: &[ToolDef],
        ) -> Result<LlmCompletion, LlmError> {
            let resp = self.complete(messages).await?;
            Ok(LlmCompletion::Done { text: resp.content, prompt_tokens: None, completion_tokens: None })
        }
    }

    #[tokio::test]
    async fn test_controller_runs_all_agents() {
        let controller = Controller::new(3, Arc::new(MockLlmProvider));
        let outputs = controller
            .run("add a hello world function")
            .await
            .unwrap();
        assert_eq!(outputs.len(), 4);
        assert!(outputs.iter().all(|o| o.success));
    }

    #[tokio::test]
    async fn test_agent_roles() {
        let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider);
        let guard = Arc::new(ExecutionGuard::default());

        assert_eq!(LlmPlannerAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Planner);
        assert_eq!(
            LlmCoderAgent {
                llm: Arc::clone(&llm),
                registry: Arc::new(crate::tools::ToolRegistry::empty()),
                guard,
                obs: crate::observability::ObsCtx::noop(),
            }
            .role(),
            AgentRole::Coder
        );
        assert_eq!(LlmRiskAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Risk);
        assert_eq!(LlmReviewerAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Reviewer);
    }
}
