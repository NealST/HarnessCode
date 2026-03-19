//! The cybernetic Controller — schedules agents and enforces pipeline-level rules.
//!
//! The Controller is the "safety harness" around the multi-agent pipeline:
//! * Drives the Planner → Coder → Risk → Reviewer sequence.
//! * Enforces retry limits and step budgets.
//! * Constructs a fresh [`ExecutionGuard`] for each attempt so resource budgets
//!   reset cleanly between retries.
//! * Handles agent errors with retry strategies.

use super::events::PipelineEvent;
use super::guardrails::ExecutionGuard;
use crate::agents::{
    drift_judge::{DriftCallback, DriftConfig, DriftJudgeAgent, DriftParams},
    Agent, AgentError, AgentOutput, AgentRole, LlmCoderAgent, LlmPlannerAgent, LlmReviewerAgent,
    LlmRiskAgent,
};
use crate::llm::LlmProvider;
use crate::observability::{
    NoopSink, ObsCtx, SpanKind, SpanSink, SpanStatus, SpanTimer, TokenUsage,
};
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

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
    /// Optional drift-detection judge.  `None` disables drift detection.
    pub drift_judge: Option<Arc<DriftJudgeAgent>>,
    /// Drift detection configuration (only used when `drift_judge` is `Some`).
    pub drift_config: DriftConfig,
}

/// Sharable configuration for [`ExecutionGuard`] construction.
#[derive(Clone)]
pub struct ExecutionGuardTemplate {
    pub max_tool_turns: usize,
}

impl Default for ExecutionGuardTemplate {
    fn default() -> Self {
        Self {
            max_tool_turns: 100,
        }
    }
}

impl ExecutionGuardTemplate {
    fn build(&self) -> ExecutionGuard {
        ExecutionGuard::new(self.max_tool_turns)
    }

    /// Build a guard for the Planner's read-only exploration loop.
    ///
    /// No step budget — the Planner only has sensor tools and cannot modify
    /// anything, so the worst case is extra token spend (bounded by the LLM
    /// provider's own limits).  Only dedup applies.
    fn build_planner(&self) -> ExecutionGuard {
        ExecutionGuard::unlimited()
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
            drift_judge: None,
            drift_config: DriftConfig::default(),
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
            drift_judge: None,
            drift_config: DriftConfig::default(),
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

    /// Attach a drift-detection judge.  Returns `self` for builder-style chaining.
    ///
    /// The `judge` can use a different (e.g. cheaper) LLM model from the main pipeline.
    ///
    /// ```rust,ignore
    /// let controller = Controller::new(3, main_llm)
    ///     .with_drift(DriftJudgeAgent { llm: cheap_llm }, DriftConfig::default());
    /// ```
    pub fn with_drift(mut self, judge: DriftJudgeAgent, config: DriftConfig) -> Self {
        self.drift_judge = Some(Arc::new(judge));
        self.drift_config = config;
        self
    }

    /// Override the maximum tool-call turns per agent execution.
    ///
    /// ```rust,ignore
    /// let controller = Controller::new(3, llm)
    ///     .with_max_tool_turns(50);
    /// ```
    pub fn with_max_tool_turns(mut self, max: usize) -> Self {
        self.guard_template.max_tool_turns = max;
        self
    }

    /// Run the pipeline for `prompt`, returning outputs from all four stages.
    pub async fn run(&self, prompt: &str) -> Result<Vec<AgentOutput>, AgentError> {
        self.run_with_progress(prompt, None, None).await
    }

    /// Run the pipeline, streaming [`PipelineEvent`]s to `tx` for live UI updates.
    ///
    /// `drift_callback` is an optional async callback that is invoked when drift is
    /// detected during the Coder stage.  Pass `None` to disable live drift interaction
    /// (drift detection itself still runs if a judge was attached via [`with_drift`]).
    pub async fn run_with_progress(
        &self,
        prompt: &str,
        tx: Option<mpsc::Sender<PipelineEvent>>,
        drift_callback: Option<DriftCallback>,
    ) -> Result<Vec<AgentOutput>, AgentError> {
        macro_rules! send {
            ($event:expr) => {
                if let Some(ref tx) = tx {
                    let _ = tx.send($event).await;
                }
            };
        }

        /// Emit a `NetworkError` event when the error originates from an LLM
        /// network / API failure.  Called before `PipelineFailed` so the user
        /// receives the specific diagnosis first.
        macro_rules! maybe_send_network_error {
            ($err:expr, $role:expr) => {
                if let AgentError::Provider(ref llm_err) = $err {
                    send!(PipelineEvent::NetworkError {
                        category: llm_err.error_category().to_string(),
                        message: llm_err.user_message(),
                        role: $role,
                    });
                }
            };
        }

        // ── Observability root ────────────────────────────────────────────
        let obs = ObsCtx::new_run(Arc::clone(&self.obs_sink));
        let pipeline_timer = SpanTimer::start();
        let pipeline_span_id = pipeline_timer.id;

        // Track whether we already used our one timeout-retry.
        let mut attempt = 0usize;
        let result: Result<Vec<AgentOutput>, AgentError>;

        // The effective prompt may be reinforced on a drift-triggered restart.
        // `original_prompt` is fixed for the lifetime of this call so the judge
        // always receives the true user intent, never the reinforced variant.
        let original_prompt = prompt.to_string();
        let mut effective_prompt = original_prompt.clone();

        // Accumulate total tokens across all agents across all attempts.
        let mut pipeline_tokens = TokenUsage::default();

        loop {
            attempt += 1;
            info!(attempt, "Starting pipeline run");

            let guard = Arc::new(self.guard_template.build());

            // Coder's ObsCtx is a child of the pipeline span so its ToolTurn/
            // ToolCall children nest correctly in the span tree.
            let coder_obs = obs.child(pipeline_span_id);

            // Construct agents.
            let sensor_registry = Arc::new(crate::tools::ToolRegistry::with_sensors());
            let planner_guard   = Arc::new(self.guard_template.build_planner());
            let planner_obs     = obs.child(pipeline_span_id);
            let planner = LlmPlannerAgent {
                llm:      Arc::clone(&self.llm),
                registry: sensor_registry,
                guard:    planner_guard,
                obs:      planner_obs,
            };
            // Build drift params for the coder if a judge and callback are available.
            let coder_drift = self.drift_judge.as_ref()
                .zip(drift_callback.as_ref())
                .map(|(judge, callback)| DriftParams {
                    judge: Arc::clone(judge),
                    config: self.drift_config.clone(),
                    // Always use the immutable original prompt so the judge
                    // sees the user's real intent even after a restart.
                    original_prompt: original_prompt.clone(),
                    callback: Arc::clone(callback),
                });
            let coder      = LlmCoderAgent    { llm: Arc::clone(&self.llm), registry: Arc::clone(&self.registry), guard: Arc::clone(&guard), obs: coder_obs, drift: coder_drift };
            let risk_agent = LlmRiskAgent     { llm: Arc::clone(&self.llm) };
            let reviewer   = LlmReviewerAgent { llm: Arc::clone(&self.llm) };

            // ── Stage 1: Planning ────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Planner });
            let stage_timer = SpanTimer::start();
            let plan_result = planner.execute(&effective_prompt).await;
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
                    maybe_send_network_error!(e, AgentRole::Planner);
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
            // ── Emit PlanReady so consumers can show the todo list ──────────────────
            {
                let steps: Vec<String> = plan.payload
                    .get("steps")
                    .and_then(|s| s.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                if !steps.is_empty() {
                    let affected_files: Vec<String> = plan.payload
                        .get("affected_files")
                        .and_then(|af| af.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    let complexity = plan.payload
                        .get("complexity")
                        .and_then(|c| c.as_str())
                        .unwrap_or("medium")
                        .to_string();
                    send!(PipelineEvent::PlanReady { steps, affected_files, complexity });
                }
            }
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
                // Drift restart is also a retry, not an error.
                Err(AgentError::DriftRestart { .. }) =>
                    SpanStatus::Retried { reason: "drift restart".into() },
                Err(e) => SpanStatus::Error { message: e.to_string() },
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Coder, attempt }, code_span_status, code_tokens));
            let code_output = match code_result {
                Err(AgentError::DriftAborted) => {
                    warn!("Pipeline aborted by user after drift detection");
                    send!(PipelineEvent::PipelineFailed {
                        error: "Pipeline aborted by user after drift detection".into()
                    });
                    result = Err(AgentError::DriftAborted);
                    break;
                }
                Err(AgentError::DriftRestart { reinforced_prompt }) => {
                    warn!("Drift restart requested — reinforcing prompt and retrying");
                    effective_prompt = reinforced_prompt;
                    if attempt >= self.max_retries {
                        send!(PipelineEvent::PipelineFailed {
                            error: "Drift restart attempted but retry limit reached".into()
                        });
                        result = Err(AgentError::MaxRetriesExceeded(self.max_retries));
                        break;
                    }
                    send!(PipelineEvent::PipelineFailed {
                        error: "Drift detected — restarting with reinforced prompt".into()
                    });
                    continue;
                }
                Err(e) => {
                    warn!(error = %e, "Coder failed");
                    maybe_send_network_error!(e, AgentRole::Coder);
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
                    maybe_send_network_error!(e, AgentRole::Reviewer);
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

        assert_eq!(
            LlmPlannerAgent {
                llm:      Arc::clone(&llm),
                registry: Arc::new(crate::tools::ToolRegistry::with_sensors()),
                guard:    Arc::clone(&guard),
                obs:      crate::observability::ObsCtx::noop(),
            }.role(),
            AgentRole::Planner,
        );
        assert_eq!(
            LlmCoderAgent {
                llm: Arc::clone(&llm),
                registry: Arc::new(crate::tools::ToolRegistry::empty()),
                guard,
                obs: crate::observability::ObsCtx::noop(),
                drift: None,
            }
            .role(),
            AgentRole::Coder
        );
        assert_eq!(LlmRiskAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Risk);
        assert_eq!(LlmReviewerAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Reviewer);
    }
}
