//! The cybernetic Controller — schedules agents and enforces pipeline-level rules.
//!
//! The Controller is the "safety harness" around the multi-agent pipeline:
//! * Drives the Judge → Scoper → Planner → Conductor → Risk → Reviewer sequence.
//! * Enforces retry limits and step budgets.
//! * Constructs a fresh [`ExecutionGuard`] for each attempt so resource budgets
//!   reset cleanly between retries.
//! * Handles agent errors with retry strategies.

use super::events::PipelineEvent;
use super::events::PhaseSummary;
use super::guardrails::ExecutionGuard;
use super::interaction::{ClarificationCallback, ClarificationRequest, ClarificationResolution};
use super::interaction::{ScoperSkipCallback, ScoperSkipDecision};
use super::request_context::RequestContext;
use crate::commands::BuiltinAgentKind;
use crate::agents::{
    compactor::CompactorAgent,
    drift_judge::{DriftCallback, DriftConfig, DriftDecision, DriftJudgeAgent, DriftNotifyFn, DriftParams},
    Agent, AgentError, AgentOutput, AgentRole, JudgeAgent, LlmConductorAgent, LlmPlannerAgent,
    LlmReviewerAgent, LlmRiskAgent, LlmScoperAgent,
};
use crate::llm::LlmProvider;
use crate::memory::{apply_patch, ConversationTurn, RECENT_TURNS_WINDOW, SessionMemory, SessionMemoryPatch, SessionStore};
use crate::controller::request_context::{ConversationMessage, ConversationRole};
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
/// 1. **Judge** — Decide whether the request needs clarification, scoping, or direct planning.
/// 2. **Frame** — Scoper defines the real problem, its boundaries, and success conditions when needed.
/// 3. **Test** — Planner evaluates the current state vs. the desired goal.
/// 4. **Operate** — Coder applies changes (with tool calls + guardrails).
/// 5. **Test** — Reviewer checks the result against success criteria.
/// 6. **Exit** — Loop terminates on success or after `max_retries` failures.
pub struct Controller {
    /// Maximum number of full pipeline retries before giving up.
    pub max_retries: usize,
    /// The LLM backend shared by all agents in the pipeline.
    pub llm: Arc<dyn LlmProvider>,
    /// The tool registry available to the Conductor during tool-use turns.
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
    /// Optional multi-session memory store.
    pub memory_store: Option<Arc<dyn SessionStore>>,
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
    /// Capped at 50 tool turns — enough headroom for large cross-module
    /// exploration tasks while providing a circuit-breaker against runaway
    /// loops.  The Planner only uses sensor (read) tools so the guard's
    /// dedup policy also applies.
    fn build_planner(&self) -> ExecutionGuard {
        ExecutionGuard::new(50)
    }

    /// Build a guard for the Scoper's read-only framing loop.
    fn build_scoper(&self) -> ExecutionGuard {
        ExecutionGuard::new(self.max_tool_turns.min(20))
    }
}

fn synthetic_scope(request_context: &RequestContext, effective_request: &str) -> AgentOutput {
    let prompt = effective_request.trim();
    let relevant_files = request_context.session_state.known_relevant_files.clone();
    let inherited_success = request_context
        .session_state
        .last_scope
        .as_ref()
        .map(|scope| payload_strings(scope, "success_criteria"))
        .filter(|criteria| !criteria.is_empty())
        .unwrap_or_else(|| vec!["The implementation matches the stated request".to_string()]);
    let assumptions = if request_context.has_meaningful_context() {
        vec!["Interpret the request together with the supplied conversation and session state"]
    } else {
        vec!["The user request is already sufficiently specific"]
    };
    // Infer task_type from keywords so that session memory carries a useful value
    // instead of always "other" when the Scoper is bypassed.
    let inferred_task_type = {
        let lower = prompt.to_lowercase();
        if lower.contains("fix") || lower.contains("bug") || lower.contains("error") || lower.contains("crash") {
            "bugfix"
        } else if lower.contains("refactor") || lower.contains("clean") || lower.contains("rename") || lower.contains("move") {
            "refactor"
        } else if lower.contains("doc") || lower.contains("comment") || lower.contains("readme") {
            "docs"
        } else if lower.contains("add") || lower.contains("implement") || lower.contains("create") || lower.contains("support") || lower.contains("test") {
            "feature"
        } else if lower.contains("review") || lower.contains("audit") || lower.contains("check") {
            "review"
        } else {
            "other"
        }
    };
    AgentOutput {
        role: AgentRole::Scoper,
        summary: "Problem framed from a well-specified request".to_string(),
        payload: serde_json::json!({
            "task_type": inferred_task_type,
            "objective": prompt,
            "problem_statement": prompt,
            "in_scope": ["Implement the user request exactly as stated"],
            "out_of_scope": ["Unrelated refactors", "Behavior changes outside the stated request"],
            "constraints": [],
            "assumptions": assumptions,
            "unknowns": request_context.session_state.open_questions,
            "relevant_files": relevant_files,
            "success_criteria": inherited_success,
            "needs_user_clarification": false,
            "clarifying_questions": [],
            "confidence": "high"
        }),
        success: true,
        tokens: None,
    }
}

fn payload_strings(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
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
            memory_store: None,
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
            memory_store: None,
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

    /// Attach a multi-session memory store.
    pub fn with_memory(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.memory_store = Some(store);
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

    /// Run the pipeline for `prompt`, returning outputs from all executed stages.
    pub async fn run(&self, prompt: &str) -> Result<Vec<AgentOutput>, AgentError> {
        self.run_with_request_context(&RequestContext::from_prompt(prompt), None, None, None, None)
            .await
    }

    /// Run the pipeline for an already-assembled request context.
    pub async fn run_with_context(
        &self,
        request_context: &RequestContext,
    ) -> Result<Vec<AgentOutput>, AgentError> {
        self.run_with_request_context(request_context, None, None, None, None).await
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
        self.run_with_request_context(&RequestContext::from_prompt(prompt), tx, drift_callback, None, None)
            .await
    }

    /// Run the pipeline using a full request context, streaming [`PipelineEvent`]s to `tx`.
    pub async fn run_with_request_context(
        &self,
        request_context: &RequestContext,
        tx: Option<mpsc::Sender<PipelineEvent>>,
        drift_callback: Option<DriftCallback>,
        clarification_callback: Option<ClarificationCallback>,
        scoper_skip_callback: Option<ScoperSkipCallback>,
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
        let mut request_context = request_context.clone();
        let mut session_memory = self.load_session_memory(&mut request_context).await?;
        let prompt = request_context.current_request.clone();
        let obs = ObsCtx::new_run(Arc::clone(&self.obs_sink));
        let pipeline_timer = SpanTimer::start();
        let pipeline_span_id = pipeline_timer.id;

        // Track whether we already used our one timeout-retry.
        let mut attempt = 0usize;
        // LOGIC-9 fix: drift restarts have their own counter (reset here,
        // not inside the loop) so they don't consume normal retry slots.
        let mut drift_restart_count = 0usize;
        let result: Result<Vec<AgentOutput>, AgentError>;

        // The effective prompt may be reinforced on a drift-triggered restart.
        // `original_prompt` is fixed for the lifetime of this call so the judge
        // always receives the true user intent, never the reinforced variant.
        let original_prompt = prompt.clone();

        // Accumulate total tokens across all agents across all attempts.
        let mut pipeline_tokens = TokenUsage::default();

        let sensor_registry = Arc::new(crate::tools::ToolRegistry::with_sensors());
        let judge = JudgeAgent {
            llm: Arc::clone(&self.llm),
        };
        // BUG-6 fix: create a fresh LlmScoperAgent (with a fresh guard) for every
        // scoper.execute() call so retries and clarification re-runs start with a
        // full turn budget instead of a depleted one.
        let make_scoper = || LlmScoperAgent {
            llm: Arc::clone(&self.llm),
            registry: Arc::clone(&sensor_registry),
            guard: Arc::new(self.guard_template.build_scoper()),
            obs: obs.child(pipeline_span_id),
        };

        // ── Stage 0: Judge ───────────────────────────────────────────────
        send!(PipelineEvent::StageStarted { role: AgentRole::Judge });
        let (judge_output, scoper_input, effective_request_override, should_run_scoper) = {
            let mut clarification_round = 0usize;

            loop {
                let stage_timer = SpanTimer::start();
                let judge_result = judge
                    .judge(&serde_json::to_string(&request_context)?)
                    .await;
                let judge_tokens = judge_result.as_ref().ok().and_then(|decision| decision.tokens);
                if let Some(t) = judge_tokens {
                    pipeline_tokens.add(t);
                }
                let judge_span_status = match &judge_result {
                    Ok(_) => SpanStatus::Ok,
                    Err(e) => SpanStatus::Error { message: e.to_string() },
                };
                obs.record(stage_timer.finish(
                    obs.run_id,
                    Some(pipeline_span_id),
                    SpanKind::Stage { role: AgentRole::Judge, attempt: clarification_round + 1 },
                    judge_span_status,
                    judge_tokens,
                ));

                let decision = match judge_result {
                    Ok(decision) => decision,
                    Err(e) => {
                        maybe_send_network_error!(e, AgentRole::Judge);
                        send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                        result = Err(e);
                        let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                        obs.record(pipeline_timer.finish(
                            obs.run_id,
                            None,
                            SpanKind::Pipeline { prompt: prompt.clone() },
                            pipeline_status,
                            Some(pipeline_tokens),
                        ));
                        obs.flush();
                        return result;
                    }
                };

                let judge_output = AgentOutput {
                    role: AgentRole::Judge,
                    summary: format!("route={} reason={}", decision.route, decision.route_reason_code),
                    payload: decision.raw.clone(),
                    success: true,
                    tokens: decision.tokens,
                };
                send!(PipelineEvent::StageCompleted {
                    output: judge_output.clone(),
                });
                send!(PipelineEvent::JudgeReady {
                    route: decision.route.clone(),
                    route_reason_code: decision.route_reason_code.clone(),
                    ready_for_scoper: decision.ready_for_scoper,
                    ready_for_planner: decision.ready_for_planner,
                    ask_user_clarification: decision.ask_user_clarification,
                    effective_request: decision.effective_request.clone(),
                    goal_is_concrete: decision.decision_factors.goal_is_concrete,
                    constraints_are_stable: decision.decision_factors.constraints_are_stable,
                    history_resolves_references: decision.decision_factors.history_resolves_references,
                    repository_grounding_needed: decision.decision_factors.repository_grounding_needed,
                    prior_scope_can_be_reused: decision.decision_factors.prior_scope_can_be_reused,
                    skip_scoper_criteria_met: decision.skip_scoper_criteria_met.clone(),
                    missing_information: decision.missing_information.clone(),
                    clarifying_questions: decision.clarifying_questions.clone(),
                    confidence: decision.confidence.clone(),
                });

                self.persist_memory_patch(
                    &request_context,
                    &mut session_memory,
                    SessionMemoryPatch {
                        title: Some(truncate_title(&request_context.current_request)),
                        effective_requests: vec![decision.effective_request.clone()],
                        // Do not write route/reason here: it would overwrite the semantic
                        // persistent_summary (e.g. "Scoped objective: ...") from prior turns.
                        ..SessionMemoryPatch::default()
                    },
                )
                .await?;

                if decision.ask_user_clarification {
                    let Some(callback) = clarification_callback.as_ref() else {
                        let error = format!(
                            "Judge requires clarification before continuing: {}",
                            decision.clarifying_questions.join(" | ")
                        );
                        send!(PipelineEvent::PipelineFailed { error: error.clone() });
                        result = Err(AgentError::ExecutionFailed {
                            role: AgentRole::Judge,
                            message: error,
                        });
                        let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                        obs.record(pipeline_timer.finish(
                            obs.run_id,
                            None,
                            SpanKind::Pipeline { prompt: prompt.clone() },
                            pipeline_status,
                            Some(pipeline_tokens),
                        ));
                        obs.flush();
                        return result;
                    };

                    send!(PipelineEvent::ClarificationRequested {
                        source: AgentRole::Judge,
                        objective: decision.effective_request.clone(),
                        questions: decision.clarifying_questions.clone(),
                    });
                    match callback(ClarificationRequest {
                        source: AgentRole::Judge,
                        questions: decision.clarifying_questions.clone(),
                        objective: decision.effective_request.clone(),
                    })
                    .await
                    {
                        ClarificationResolution::Abort => {
                            let error = "Pipeline aborted while waiting for clarification".to_string();
                            send!(PipelineEvent::PipelineFailed { error: error.clone() });
                            result = Err(AgentError::ExecutionFailed {
                                role: AgentRole::Judge,
                                message: error,
                            });
                            let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                            obs.record(pipeline_timer.finish(
                                obs.run_id,
                                None,
                                SpanKind::Pipeline { prompt: prompt.clone() },
                                pipeline_status,
                                Some(pipeline_tokens),
                            ));
                            obs.flush();
                            return result;
                        }
                        ClarificationResolution::Answer(answer) => {
                            request_context.apply_clarification(
                                "judge",
                                &decision.clarifying_questions,
                                answer,
                            );
                            self.persist_memory_patch(
                                &request_context,
                                &mut session_memory,
                                SessionMemoryPatch {
                                    persistent_summary: request_context.session_state.persistent_summary.clone(),
                                    clarified_facts: request_context.session_state.clarified_facts.clone(),
                                    open_questions: Some(request_context.session_state.open_questions.clone()),
                                    ..SessionMemoryPatch::default()
                                },
                            )
                            .await?;
                            clarification_round += 1;
                            if clarification_round >= 3 {
                                let error = "Too many clarification rounds requested by Judge".to_string();
                                send!(PipelineEvent::PipelineFailed { error: error.clone() });
                                result = Err(AgentError::ExecutionFailed {
                                    role: AgentRole::Judge,
                                    message: error,
                                });
                                let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                                obs.record(pipeline_timer.finish(
                                    obs.run_id,
                                    None,
                                    SpanKind::Pipeline { prompt: prompt.clone() },
                                    pipeline_status,
                                    Some(pipeline_tokens),
                                ));
                                obs.flush();
                                return result;
                            }
                            continue;
                        }
                    }
                }

                let scoper_input = serde_json::json!({
                    "request_context": request_context,
                    "effective_request": decision.effective_request,
                })
                .to_string();
                break (
                    judge_output,
                    scoper_input,
                    decision.effective_request,
                    decision.ready_for_scoper && !decision.ready_for_planner,
                );
            }
        };
        // Initialise the effective prompt from what the Judge resolved. On a
        // drift-triggered restart this gets overwritten before the next Planner call.
        let mut effective_prompt = effective_request_override.clone();

        // ── Stage 1: Scoping ──────────────────────────────────────────────
        // Give the user a chance to skip the Scoper before the LLM call.
        // The callback is invoked only when the Judge explicitly routed to Scoper.
        let should_run_scoper = if should_run_scoper {
            // MISSING-1 fix: emit StageStarted before the skip callback so consumers
            // always see a paired Start event regardless of the skip resolution.
            send!(PipelineEvent::StageStarted { role: AgentRole::Scoper });
            if let Some(ref cb) = scoper_skip_callback {
                match cb(effective_request_override.clone()).await {
                    ScoperSkipDecision::Skip => {
                        send!(PipelineEvent::StageSkipped { role: AgentRole::Scoper });
                        false
                    }
                    ScoperSkipDecision::Run => true,
                }
            } else {
                true
            }
        } else {
            // LOGIC-4 fix: Judge routed directly to Planner — emit StageSkipped so
            // consumers don’t see the Scoper stage stuck as "pending".
            send!(PipelineEvent::StageSkipped { role: AgentRole::Scoper });
            false
        };
        let stage_timer = SpanTimer::start();
        let mut scope_result = if should_run_scoper {
            Some(make_scoper().execute(&scoper_input).await)
        } else {
            None
        };
        let scope_tokens = scope_result.as_ref().and_then(|r| r.as_ref().ok()).and_then(|o| o.tokens);
        if let Some(t) = scope_tokens { pipeline_tokens.add(t); }
        let scope_span_status = match &scope_result {
            Some(Ok(o)) if o.success => SpanStatus::Ok,
            Some(Ok(o)) => SpanStatus::Retried { reason: o.summary.clone() },
            Some(Err(e)) => SpanStatus::Error { message: e.to_string() },
            None => SpanStatus::Ok,
        };
        // Only record observability span when Scoper actually ran (not when user skipped).
        if scope_result.is_some() {
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Scoper, attempt: 1 }, scope_span_status, scope_tokens));
        }
        let scoped = match scope_result {
            Some(Err(e)) => {
                maybe_send_network_error!(e, AgentRole::Scoper);
                send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                result = Err(e);
                let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                obs.record(pipeline_timer.finish(
                    obs.run_id,
                    None,
                    SpanKind::Pipeline { prompt: prompt.clone() },
                    pipeline_status,
                    Some(pipeline_tokens),
                ));
                obs.flush();
                return result;
            }
            Some(Ok(o)) if !o.success => {
                // First attempt produced a structurally invalid scope (e.g. missing
                // objective or empty success_criteria).  Allow one silent retry with
                // the same input before giving up — transient LLM formatting issues
                // should not be immediately fatal.
                warn!("Scoper attempt 1 returned !success ({}); retrying once", o.summary);
                send!(PipelineEvent::StageRetrying {
                    role: AgentRole::Scoper,
                    reason: o.summary.clone(),
                    attempt: 1,
                });
                let retry_timer = SpanTimer::start();
                let retry_result = make_scoper().execute(&scoper_input).await;
                let retry_tokens = retry_result.as_ref().ok().and_then(|o| o.tokens);
                if let Some(t) = retry_tokens { pipeline_tokens.add(t); }
                let retry_span_status = match &retry_result {
                    Ok(o) if o.success => SpanStatus::Ok,
                    Ok(o) => SpanStatus::Retried { reason: o.summary.clone() },
                    Err(e) => SpanStatus::Error { message: e.to_string() },
                };
                obs.record(retry_timer.finish(
                    obs.run_id, Some(pipeline_span_id),
                    SpanKind::Stage { role: AgentRole::Scoper, attempt: 2 },
                    retry_span_status, retry_tokens,
                ));
                match retry_result {
                    Err(e) => {
                        maybe_send_network_error!(e, AgentRole::Scoper);
                        send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                        result = Err(e);
                        let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                        obs.record(pipeline_timer.finish(obs.run_id, None, SpanKind::Pipeline { prompt: prompt.clone() }, pipeline_status, Some(pipeline_tokens)));
                        obs.flush();
                        return result;
                    }
                    Ok(retry_o) if !retry_o.success => {
                        let msg = format!("Scoper failed after 2 attempts: {}", retry_o.summary);
                        send!(PipelineEvent::PipelineFailed { error: msg.clone() });
                        result = Err(AgentError::ExecutionFailed { role: AgentRole::Scoper, message: msg });
                        let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                        obs.record(pipeline_timer.finish(obs.run_id, None, SpanKind::Pipeline { prompt: prompt.clone() }, pipeline_status, Some(pipeline_tokens)));
                        obs.flush();
                        return result;
                    }
                    Ok(retry_o) => retry_o,
                }
            }
            Some(Ok(o)) => o,
            None => synthetic_scope(&request_context, &effective_request_override),
        };
        let scoped = if scoped.payload.get("needs_user_clarification").and_then(|value| value.as_bool()).unwrap_or(false) {
            if let Some(callback) = clarification_callback.as_ref() {
                let questions = payload_strings(&scoped.payload, "clarifying_questions");
                send!(PipelineEvent::ClarificationRequested {
                    source: AgentRole::Scoper,
                    objective: scoped.payload.get("objective").and_then(|value| value.as_str()).unwrap_or(&effective_request_override).to_string(),
                    questions: questions.clone(),
                });
                match callback(ClarificationRequest {
                    source: AgentRole::Scoper,
                    questions: questions.clone(),
                    objective: scoped.payload.get("objective").and_then(|value| value.as_str()).unwrap_or(&effective_request_override).to_string(),
                }).await {
                    ClarificationResolution::Abort => {
                        let error = "Pipeline aborted while waiting for scoper clarification".to_string();
                        send!(PipelineEvent::PipelineFailed { error: error.clone() });
                        result = Err(AgentError::ExecutionFailed { role: AgentRole::Scoper, message: error });
                        let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                        obs.record(pipeline_timer.finish(
                            obs.run_id,
                            None,
                            SpanKind::Pipeline { prompt: prompt.clone() },
                            pipeline_status,
                            Some(pipeline_tokens),
                        ));
                        obs.flush();
                        return result;
                    }
                    ClarificationResolution::Answer(answer) => {
                        request_context.apply_clarification("scoper", &questions, answer);
                        self.persist_memory_patch(
                            &request_context,
                            &mut session_memory,
                            SessionMemoryPatch {
                                persistent_summary: request_context.session_state.persistent_summary.clone(),
                                clarified_facts: request_context.session_state.clarified_facts.clone(),
                                open_questions: Some(request_context.session_state.open_questions.clone()),
                                ..SessionMemoryPatch::default()
                            },
                        )
                        .await?;
                        let retry_timer = SpanTimer::start();
                        scope_result = Some(make_scoper().execute(&serde_json::json!({
                            "request_context": request_context,
                            "effective_request": effective_request_override,
                        }).to_string()).await);
                        let retry_tokens = scope_result.as_ref().and_then(|r| r.as_ref().ok()).and_then(|o| o.tokens);
                        if let Some(t) = retry_tokens { pipeline_tokens.add(t); }
                        let retry_span_status = match &scope_result {
                            Some(Ok(o)) if o.success => SpanStatus::Ok,
                            Some(Ok(o)) => SpanStatus::Retried { reason: o.summary.clone() },
                            Some(Err(e)) => SpanStatus::Error { message: e.to_string() },
                            None => SpanStatus::Ok,
                        };
                        obs.record(retry_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Scoper, attempt: 2 }, retry_span_status, retry_tokens));
                        match scope_result {
                            Some(Ok(output)) if output.success => output,
                            Some(Ok(output)) => {
                                // Second Scoper call still returned !success — give up.
                                let msg = format!("Scoper failed after clarification retry: {}", output.summary);
                                send!(PipelineEvent::PipelineFailed { error: msg.clone() });
                                result = Err(AgentError::ExecutionFailed { role: AgentRole::Scoper, message: msg });
                                let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                                obs.record(pipeline_timer.finish(obs.run_id, None, SpanKind::Pipeline { prompt: prompt.clone() }, pipeline_status, Some(pipeline_tokens)));
                                obs.flush();
                                return result;
                            }
                            Some(Err(e)) => {
                                maybe_send_network_error!(e, AgentRole::Scoper);
                                send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                                result = Err(e);
                                let pipeline_status = SpanStatus::Error { message: result.as_ref().err().map(ToString::to_string).unwrap_or_default() };
                                obs.record(pipeline_timer.finish(
                                    obs.run_id,
                                    None,
                                    SpanKind::Pipeline { prompt: prompt.clone() },
                                    pipeline_status,
                                    Some(pipeline_tokens),
                                ));
                                obs.flush();
                                return result;
                            }
                            None => unreachable!(),
                        }
                    }
                }
            } else {
                scoped
            }
        } else {
            scoped
        };
        self.persist_memory_patch(
            &request_context,
            &mut session_memory,
            SessionMemoryPatch {
                execution_summary: Some(scoped.summary.clone()),
                persistent_summary: scoped
                    .payload
                    .get("objective")
                    .and_then(|value| value.as_str())
                    .map(|objective| format!("Scoped objective: {objective}")),
                known_relevant_files: payload_strings(&scoped.payload, "relevant_files"),
                open_questions: Some(payload_strings(&scoped.payload, "clarifying_questions")),
                last_scope: Some(scoped.payload.clone()),
                ..SessionMemoryPatch::default()
            },
        )
        .await?;
        // Only emit scope events when the Scoper agent actually ran.
        // On a user-initiated skip the StageSkipped event was already sent;
        // emitting StageCompleted/ScopeReady here would cause the CLI to print
        // the synthetic problem frame even though the user chose to bypass it.
        if should_run_scoper {
            send!(PipelineEvent::StageCompleted { output: scoped.clone() });
        }
        if should_run_scoper {
            send!(PipelineEvent::ScopeReady {
                task_type: scoped.payload.get("task_type").and_then(|v| v.as_str()).unwrap_or("other").to_string(),
                objective: scoped.payload.get("objective").and_then(|v| v.as_str()).unwrap_or(prompt.as_str()).to_string(),
                in_scope: payload_strings(&scoped.payload, "in_scope"),
                out_of_scope: payload_strings(&scoped.payload, "out_of_scope"),
                unknowns: payload_strings(&scoped.payload, "unknowns"),
                success_criteria: payload_strings(&scoped.payload, "success_criteria"),
                relevant_files: payload_strings(&scoped.payload, "relevant_files"),
                needs_user_clarification: scoped.payload.get("needs_user_clarification").and_then(|v| v.as_bool()).unwrap_or(false),
                clarifying_questions: payload_strings(&scoped.payload, "clarifying_questions"),
                confidence: scoped.payload.get("confidence").and_then(|v| v.as_str()).unwrap_or("medium").to_string(),
            });
        }

        loop {
            attempt += 1;
            info!(attempt, "Starting pipeline run");

            // Compute agents needed for this attempt.
            // BUG-7 fix: use a closure so each planner.execute() call (including the
            // !success retry) starts with a fresh guard and a full turn budget.
            let make_planner = || LlmPlannerAgent {
                llm:      Arc::clone(&self.llm),
                registry: Arc::clone(&sensor_registry),
                guard:    Arc::new(self.guard_template.build_planner()),
                obs:      obs.child(pipeline_span_id),
            };
            let risk_agent = LlmRiskAgent     { llm: Arc::clone(&self.llm) };
            let reviewer   = LlmReviewerAgent { llm: Arc::clone(&self.llm) };

            // DESIGN-6: Warn when replanning with a stale scope on retry.
            if attempt > 1 {
                warn!(attempt, "Retrying with stale scope; a fresh Scoper run may improve results");
            }

            // ── Stage 1: Planning ────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Planner });
            let stage_timer = SpanTimer::start();
            let planner_input = serde_json::json!({
                "request_context": request_context,
                "user_request": original_prompt,
                    "effective_request": effective_prompt,
                "problem_frame": scoped.payload,
            }).to_string();
            let plan_result = make_planner().execute(&planner_input).await;
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
                    warn!(role = %o.role, "Planner attempt 1 returned !success ({}); retrying once", o.summary);
                    // Notify the user that a retry is happening — the spinner stays
                    // alive and its message is updated rather than finishing the stage.
                    send!(PipelineEvent::StageRetrying {
                        role: AgentRole::Planner,
                        reason: o.summary.clone(),
                        attempt: 1,
                    });
                    let retry_timer = SpanTimer::start();
                    let retry_result = make_planner().execute(&planner_input).await;
                    let retry_tokens = retry_result.as_ref().ok().and_then(|o2| o2.tokens);
                    if let Some(t) = retry_tokens { pipeline_tokens.add(t); }
                    let retry_span_status = match &retry_result {
                        Ok(o2) if o2.success => SpanStatus::Ok,
                        Ok(o2) => SpanStatus::Retried { reason: o2.summary.clone() },
                        Err(e) => SpanStatus::Error { message: e.to_string() },
                    };
                    obs.record(retry_timer.finish(
                        obs.run_id, Some(pipeline_span_id),
                        SpanKind::Stage { role: AgentRole::Planner, attempt: 2 },
                        retry_span_status, retry_tokens,
                    ));
                    match retry_result {
                        Err(e) => {
                            maybe_send_network_error!(e, AgentRole::Planner);
                            send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                            result = Err(e);
                            break;
                        }
                        Ok(o2) if !o2.success => {
                            warn!(role = %o2.role, "Planner retry also failed: {}", o2.summary);
                            if attempt >= self.max_retries {
                                send!(PipelineEvent::PipelineFailed {
                                    error: format!("Planner failed after retry: {}", o2.summary),
                                });
                                result = Err(AgentError::MaxRetriesExceeded(self.max_retries));
                                break;
                            }
                            send!(PipelineEvent::PipelineRetrying {
                                reason: format!("Planner failed: {}", o2.summary),
                                attempt: attempt + 1,
                            });
                            continue;
                        }
                        Ok(o2) => o2,
                    }
                }
                Ok(o) => o,
            };
            self.persist_memory_patch(
                &request_context,
                &mut session_memory,
                SessionMemoryPatch {
                    execution_summary: Some(plan.summary.clone()),
                    known_relevant_files: {
                        // Collect affected_files from all phases.
                        plan.payload
                            .get("phases")
                            .and_then(|p| p.as_array())
                            .map(|phases| {
                                phases
                                    .iter()
                                    .flat_map(|phase| payload_strings(phase, "affected_files"))
                                    .collect::<std::collections::HashSet<_>>()
                                    .into_iter()
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    },
                    last_plan: Some(plan.payload.clone()),
                    ..SessionMemoryPatch::default()
                },
            )
            .await?;
            send!(PipelineEvent::StageCompleted { output: plan.clone() });

            // ── Emit PlanReady so consumers can render the phase todo-list ────
            {
                let phases_arr = plan.payload
                    .get("phases")
                    .and_then(|p| p.as_array())
                    .cloned()
                    .unwrap_or_default();
                let phase_count = phases_arr.len();
                let phase_summaries: Vec<PhaseSummary> = phases_arr
                    .iter()
                    .map(|ph| PhaseSummary {
                        phase_id: ph.get("phase_id").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
                        title: ph.get("title").and_then(|v| v.as_str()).unwrap_or("(untitled)").to_string(),
                        step_count: ph.get("steps").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                        complexity: ph.get("complexity").and_then(|v| v.as_str()).unwrap_or("medium").to_string(),
                    })
                    .collect();
                // Overall complexity = highest phase complexity.
                let overall_complexity = phase_summaries
                    .iter()
                    .max_by_key(|p| match p.complexity.as_str() {
                        "high" => 2u8,
                        "medium" => 1,
                        _ => 0,
                    })
                    .map(|p| p.complexity.clone())
                    .unwrap_or_else(|| "medium".to_string());
                if phase_count > 0 {
                    send!(PipelineEvent::PlanReady {
                        phase_count,
                        phases: phase_summaries,
                        complexity: overall_complexity,
                    });
                }
            }

            // ── Stage 2: Execution (Conductor) — phase-by-phase loop ──────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Conductor });

            let phases_arr = plan.payload
                .get("phases")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            let total_phases = phases_arr.len();

            // Accumulates merged state across all completed phases.
            let mut all_phase_outputs: Vec<serde_json::Value> = Vec::new();
            // Maintained in lock-step with all_phase_outputs to avoid O(n²) rebuilds
            // inside the phase loop.
            let mut completed_phase_summaries: Vec<serde_json::Value> = Vec::new();
            // BUG-1 fix: accumulate token usage from all successful phases so
            // StageCompleted and code_output carry a real figure instead of None.
            let mut conductor_phase_tokens = TokenUsage::default();
            let mut conductor_failed = false;
            let mut conductor_error: Option<AgentError> = None;
            let mut drift_occurred = false;
            let mut drift_reinforced_prompt: Option<String> = None;

            'phases: for phase in &phases_arr {
                let phase_id = phase.get("phase_id").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let phase_title = phase.get("title").and_then(|v| v.as_str()).unwrap_or("(untitled)").to_string();

                send!(PipelineEvent::PhaseStarted {
                    phase_id,
                    title: phase_title.clone(),
                    total_phases,
                });

                let global_success = plan.payload
                    .get("global_success_criteria")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                // Build the judge prompt with the current phase objective and scope
                // boundaries so the judge can distinguish legitimate phase work from
                // true drift (DESIGN-1 fix).
                let phase_objective = phase
                    .get("objective")
                    .and_then(|v| v.as_str())
                    .unwrap_or(phase_title.as_str());
                // Scope boundaries extracted from the Scoper's output.
                let in_scope_items: Vec<&str> = scoped.payload
                    .get("in_scope")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let out_of_scope_items: Vec<&str> = scoped.payload
                    .get("out_of_scope")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let relevant_files_items: Vec<&str> = scoped.payload
                    .get("relevant_files")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                // BUG-1 fix: `judge_prompt` is phase-scoped (includes objective +
                // scope boundaries) and used ONLY for the judge LLM call.
                // `original_prompt` stays clean for drift-reinforced restarts.
                let judge_prompt = {
                    let mut s = format!(
                        "{original_prompt}\n\nCurrent phase objective: {phase_objective}"
                    );
                    if !in_scope_items.is_empty() {
                        s.push_str(&format!("\nIn scope: {}", in_scope_items.join(", ")));
                    }
                    if !out_of_scope_items.is_empty() {
                        s.push_str(&format!("\nOut of scope: {}", out_of_scope_items.join(", ")));
                    }
                    if !relevant_files_items.is_empty() {
                        s.push_str(&format!("\nExpected affected files: {}", relevant_files_items.join(", ")));
                    }
                    s
                };

                let phase_context = serde_json::json!({
                    "effective_request": effective_prompt,
                    "session_summary": request_context.session_state.persistent_summary,
                    "problem_frame": scoped.payload,
                    "global_success_criteria": global_success,
                    "completed_phases": completed_phase_summaries,
                    "current_phase": phase,
                })
                .to_string();

                // LOGIC-1 fix: when a judge is attached but no drift_callback was
                // supplied (e.g. CLI / tests), default to an Ignore-everything callback
                // so drift detection still runs and logs, rather than being silently
                // disabled by the .zip() returning None.
                let effective_drift_callback: Option<DriftCallback> = if self.drift_judge.is_some() {
                    Some(drift_callback.as_ref().cloned().unwrap_or_else(|| {
                        Arc::new(|_signal| Box::pin(async { DriftDecision::Ignore }))
                    }))
                } else {
                    None
                };

                // LOGIC-3 / MISSING-1: build a synchronous notify hook that fires a
                // DriftDetected event on the pipeline channel before the async callback
                // is awaited, so the UI can render a warning immediately.
                let drift_notify: Option<DriftNotifyFn> =
                    effective_drift_callback.as_ref().and_then(|_| {
                        tx.as_ref().map(|sender| {
                            let sender = sender.clone();
                            Arc::new(move |kind: &crate::agents::drift_judge::DriftKind, reason: &str| {
                                let tx = sender.clone();
                                let event = PipelineEvent::DriftDetected {
                                    kind: kind.to_string(),
                                    reason: reason.to_string(),
                                };
                                tokio::spawn(async move { let _ = tx.send(event).await; });
                            }) as DriftNotifyFn
                        })
                    });

                // Each phase gets its own fresh guard and ObsCtx child span.
                let make_conductor = || {
                    let g   = Arc::new(self.guard_template.build());
                    let obs = obs.child(pipeline_span_id);
                    let drift = self.drift_judge.as_ref()
                        .zip(effective_drift_callback.as_ref())
                        .map(|(judge, callback)| DriftParams {
                            judge: Arc::clone(judge),
                            config: self.drift_config.clone(),
                            original_prompt: original_prompt.clone(),
                            judge_prompt: judge_prompt.clone(),
                            callback: Arc::clone(callback),
                            notify: drift_notify.clone(),
                        });
                    LlmConductorAgent {
                        llm:      Arc::clone(&self.llm),
                        registry: Arc::clone(&self.registry),
                        guard:    g,
                        obs,
                        drift,
                    }
                };

                // Allow one silent retry per phase (mirrors Planner retry policy).
                // The retry receives a fresh guard so the full step budget is
                // available, and the context is augmented with the failure reason.
                let phase_timer = SpanTimer::start();
                let mut phase_span_tokens = TokenUsage::default();
                let mut phase_result = make_conductor().execute(&phase_context).await;
                let phase1_tokens = phase_result.as_ref().ok().and_then(|o| o.tokens);
                if let Some(t) = phase1_tokens { pipeline_tokens.add(t); phase_span_tokens.add(t); }

                let needs_retry = matches!(&phase_result, Ok(o) if !o.success);
                if needs_retry {
                    let failure_reason = phase_result.as_ref().unwrap().summary.clone();
                    send!(PipelineEvent::PhaseRetrying {
                        phase_id,
                        title: phase_title.clone(),
                        reason: failure_reason.clone(),
                        attempt: 1,
                    });
                    // Build a retry context that explains why the first attempt failed.
                    let retry_context = serde_json::json!({
                        "effective_request": effective_prompt,
                        "session_summary": request_context.session_state.persistent_summary,
                        "problem_frame": scoped.payload,
                        "global_success_criteria": global_success,
                        "completed_phases": completed_phase_summaries,
                        "current_phase": phase,
                        "previous_attempt_result": {
                            "success_criteria_met": false,
                            "explanation": failure_reason,
                        },
                    })
                    .to_string();
                    phase_result = make_conductor().execute(&retry_context).await;
                    let phase2_tokens = phase_result.as_ref().ok().and_then(|o| o.tokens);
                    if let Some(t) = phase2_tokens { pipeline_tokens.add(t); phase_span_tokens.add(t); }
                }

                let phase_span_status = match &phase_result {
                    Ok(o) if o.success   => SpanStatus::Ok,
                    Ok(o)                => SpanStatus::Error { message: o.summary.clone() },
                    Err(AgentError::DriftRestart { .. }) => SpanStatus::Retried { reason: "drift restart".into() },
                    Err(e)               => SpanStatus::Error { message: e.to_string() },
                };
                // Use phase_id as the attempt index so each phase is distinguishable
                // in the observability log (the outer `attempt` is the pipeline-retry counter).
                obs.record(phase_timer.finish(obs.run_id, Some(pipeline_span_id),
                    SpanKind::Stage { role: AgentRole::Conductor, attempt: phase_id },
                    phase_span_status, Some(phase_span_tokens)));

                match phase_result {
                    Err(AgentError::DriftAborted) => {
                        warn!("Pipeline aborted by user after drift detection in phase {phase_id}");
                        send!(PipelineEvent::PipelineFailed {
                            error: "Pipeline aborted by user after drift detection".into()
                        });
                        conductor_error = Some(AgentError::DriftAborted);
                        conductor_failed = true;
                        drift_occurred = true;
                        break 'phases;
                    }
                    Err(AgentError::DriftRestart { reinforced_prompt }) => {
                        warn!("Drift restart requested during phase {phase_id}");
                        drift_reinforced_prompt = Some(reinforced_prompt);
                        conductor_failed = true;
                        drift_occurred = true;
                        break 'phases;
                    }
                    Err(e) => {
                        warn!(error = %e, "Conductor failed on phase {phase_id}");
                        maybe_send_network_error!(e, AgentRole::Conductor);
                        send!(PipelineEvent::PhaseFailed {
                            phase_id,
                            title: phase_title.clone(),
                            reason: e.to_string(),
                        });
                        conductor_error = Some(e);
                        conductor_failed = true;
                        break 'phases;
                    }
                    Ok(o) if !o.success => {
                        warn!("Conductor phase {phase_id} still failing after retry: {}", o.summary);
                        send!(PipelineEvent::PhaseFailed {
                            phase_id,
                            title: phase_title.clone(),
                            reason: o.summary.clone(),
                        });
                        // BUG-3 fix: don’t emit PipelineFailed here — the outer retry loop
                        // decides whether to retry or give up. PipelineFailed is terminal;
                        // emitting it prematurely breaks consumers when retries remain.
                        conductor_error = Some(AgentError::ExecutionFailed {
                            role: AgentRole::Conductor,
                            message: format!("Phase {phase_id} ({phase_title}) failed: {}", o.summary),
                        });
                        conductor_failed = true;
                        break 'phases;
                    }
                    Ok(mut o) => {
                        // Correct phase_id if the LLM returned a mismatched value.
                        let returned_id = o.payload
                            .get("phase_id")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        if returned_id != phase_id {
                            warn!(
                                returned_id,
                                expected = phase_id,
                                "Conductor returned wrong phase_id; correcting"
                            );
                            o.payload["phase_id"] = serde_json::json!(phase_id);
                        }
                        let files_changed = o.payload
                            .get("files_changed")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let affected_files: Vec<String> = o.payload
                            .get("affected_files")
                            .and_then(|v| v.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                            .unwrap_or_default();
                        send!(PipelineEvent::PhaseCompleted {
                            phase_id,
                            title: phase_title.clone(),
                            total_phases,
                            explanation: o.summary.clone(),
                            files_changed,
                            affected_files,
                        });
                        // Persist phase progress to session memory after each phase.
                        self.persist_memory_patch(
                            &request_context,
                            &mut session_memory,
                            SessionMemoryPatch {
                                execution_summary: Some(format!(
                                    "Phase {phase_id}/{total_phases} ({phase_title}): {}",
                                    o.summary
                                )),
                                ..SessionMemoryPatch::default()
                            },
                        )
                        .await?;
                        completed_phase_summaries.push(serde_json::json!({
                            "phase_id":       o.payload.get("phase_id"),
                            "explanation":    o.payload.get("explanation"),
                            "files_changed":  o.payload.get("files_changed"),
                            "affected_files": o.payload.get("affected_files"),
                        }));
                        // BUG-1 fix: accumulate per-phase tokens.
                        // TokenUsage is Copy so phase_span_tokens is still valid here.
                        conductor_phase_tokens.add(phase_span_tokens);
                        all_phase_outputs.push(o.payload.clone());
                    }
                }
            }

            // Handle conductor failure / drift before proceeding to Risk + Review.
            if conductor_failed {
                if drift_occurred {
                    if let Some(reinforced_prompt) = drift_reinforced_prompt {
                        effective_prompt = reinforced_prompt;
                        // LOGIC-9 fix: drift restarts have their own budget
                        // (max_drift_restarts) so they don't consume the normal
                        // phase-failure retry slots.
                        drift_restart_count += 1;
                        if drift_restart_count > self.drift_config.max_drift_restarts {
                            send!(PipelineEvent::PipelineFailed {
                                error: format!(
                                    "Drift restart limit ({}) exceeded",
                                    self.drift_config.max_drift_restarts
                                ),
                            });
                            result = Err(AgentError::MaxRetriesExceeded(
                                self.drift_config.max_drift_restarts,
                            ));
                            break;
                        }
                        send!(PipelineEvent::PipelineRetrying {
                            reason: "Drift detected — restarting with reinforced prompt".into(),
                            attempt: drift_restart_count,
                        });
                        continue;
                    }
                    // DriftAborted — PipelineFailed already emitted inside the phase loop.
                    result = Err(conductor_error.unwrap_or(AgentError::DriftAborted));
                    break;
                }
                // Normal phase failure — consume a retry slot.
                if attempt >= self.max_retries {
                    let err_msg = conductor_error.as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "Max retries exceeded".to_string());
                    send!(PipelineEvent::PipelineFailed { error: err_msg });
                    result = Err(conductor_error.unwrap_or_else(|| AgentError::MaxRetriesExceeded(self.max_retries)));
                    break;
                }
                // LOGIC-5 fix: signal that a retry is starting so consumers don’t jump
                // from PhaseFailed directly to the next StageStarted silently.
                send!(PipelineEvent::PipelineRetrying {
                    reason: conductor_error.as_ref().map(|e| e.to_string())
                        .unwrap_or_else(|| "Phase execution failed".to_string()),
                    attempt: attempt + 1,
                });
                continue;
            }

            // BUG-1 fix: use the accumulated conductor_phase_tokens instead of trying
            // to read _prompt_tokens/_completion_tokens from JSON payloads (those fields
            // don’t exist — tokens live on AgentOutput.tokens, not the LLM JSON).
            let conductor_total_tokens = if conductor_phase_tokens.prompt > 0
                || conductor_phase_tokens.completion > 0
            {
                Some(conductor_phase_tokens)
            } else {
                None
            };
            send!(PipelineEvent::StageCompleted {
                output: AgentOutput {
                    role: AgentRole::Conductor,
                    summary: format!("All {} phase(s) completed", total_phases),
                    payload: serde_json::json!({
                        "phases": all_phase_outputs,
                        "total_phases": total_phases,
                    }),
                    success: true,
                    tokens: conductor_total_tokens,
                }
            });

            // Build a merged code_output for downstream Risk and Reviewer agents.
            // Concatenate all per-phase diffs and sum file counts.
            let merged_diff: String = all_phase_outputs
                .iter()
                .filter_map(|o| o.get("diff").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            let merged_files_changed: u64 = all_phase_outputs
                .iter()
                .filter_map(|o| o.get("files_changed").and_then(|v| v.as_u64()))
                .sum();
            let merged_explanation: String = all_phase_outputs
                .iter()
                .enumerate()
                .filter_map(|(i, o)| {
                    let pid = o.get("phase_id").and_then(|v| v.as_u64()).unwrap_or((i + 1) as u64);
                    o.get("explanation")
                        .and_then(|v| v.as_str())
                        .map(|s| format!("Phase {pid}: {s}"))
                })
                .collect::<Vec<_>>()
                .join("; ");
            // Aggregate affected_files and delta_reason across all phases so the
            // Reviewer's file-scope drift check has the data it needs.
            let mut merged_affected_files: Vec<String> = all_phase_outputs
                .iter()
                .flat_map(|o| {
                    o.get("affected_files")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            merged_affected_files.sort();
            let merged_delta_reason: String = all_phase_outputs
                .iter()
                .filter_map(|o| {
                    o.get("affected_files_delta_reason")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                })
                .collect::<Vec<_>>()
                .join("; ");
            // Aggregate actual_changes across all phases so Risk receives the
            // ground-truth write/patch history at the top level of its payload —
            // not buried inside phase_outputs.
            let merged_actual_changes: Vec<serde_json::Value> = all_phase_outputs
                .iter()
                .flat_map(|o| {
                    o.get("actual_changes")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            let code_output = AgentOutput {
                role: AgentRole::Conductor,
                summary: merged_explanation.clone(),
                payload: serde_json::json!({
                    "diff": merged_diff,
                    "files_changed": merged_files_changed,
                    "explanation": merged_explanation,
                    "affected_files": merged_affected_files,
                    "affected_files_delta_reason": merged_delta_reason,
                    "actual_changes": merged_actual_changes,
                    "phase_outputs": all_phase_outputs,
                }),
                success: true,
                // LOGIC-3 fix: use the accumulated conductor_phase_tokens.
                tokens: if conductor_phase_tokens.prompt > 0 || conductor_phase_tokens.completion > 0 {
                    Some(conductor_phase_tokens)
                } else {
                    None
                },
            };

            // ── Stage 3: Risk (informational, never retries) ─────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Risk });
            let stage_timer = SpanTimer::start();
            // Build a slimmed payload for Risk: omit `phase_outputs` to avoid
            // duplicating `actual_changes` (which is already at the top level)
            // and prevent blowing the LLM context budget on data it doesn't use.
            let risk_payload = serde_json::json!({
                "diff":            code_output.payload["diff"],
                "explanation":     code_output.payload["explanation"],
                "affected_files":  code_output.payload["affected_files"],
                "actual_changes":  code_output.payload["actual_changes"],
            });
            // DESIGN-4 fix: also consider files_changed>0 so that changes applied via
            // run_command (sed -i, cargo fmt, etc.) are not silently skipped by Risk.
            // If nothing was genuinely changed, skip the LLM call and emit risk_unavailable
            // rather than letting the LLM hallucinate a "low" verdict from an empty payload.
            let no_changes = code_output.payload["actual_changes"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true)
                && code_output.payload["diff"]
                    .as_str()
                    .map(str::is_empty)
                    .unwrap_or(true)
                && code_output.payload["files_changed"]
                    .as_u64()
                    .unwrap_or(0) == 0;
            let risk_result: Result<AgentOutput, AgentError> = if no_changes {
                warn!("Risk skipped: no actual file changes recorded");
                Ok(AgentOutput {
                    role: AgentRole::Risk,
                    summary: "[SKIPPED] No file changes to assess".to_string(),
                    payload: serde_json::json!({
                        "risk_level": "unknown",
                        "reason": "No file changes were recorded for this run.",
                        "risk_unavailable": true,
                    }),
                    success: false,
                    tokens: None,
                })
            } else {
                let risk_context = serde_json::to_string(&risk_payload)?;
                risk_agent.execute(&risk_context).await
            };
            let risk_tokens = risk_result.as_ref().ok().and_then(|o| o.tokens);
            if let Some(t) = risk_tokens { pipeline_tokens.add(t); }
            let risk_span_status = if no_changes {
                SpanStatus::Skipped { reason: "no file changes recorded".to_string() }
            } else {
                match &risk_result {
                    Ok(_) => SpanStatus::Ok,
                    Err(e) => SpanStatus::Error { message: e.to_string() },
                }
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Risk, attempt }, risk_span_status, risk_tokens));
            let risk_output = risk_result.unwrap_or_else(|e| {
                warn!(error = %e, "Risk agent failed (non-fatal, continuing)");
                AgentOutput {
                    role: AgentRole::Risk,
                    summary: format!("Risk assessment unavailable: {e}"),
                    payload: serde_json::json!({
                        "risk_level": "unknown",
                        "reason": e.to_string(),
                        "risk_unavailable": true,
                    }),
                    success: false,
                    tokens: None,
                }
            });
            send!(PipelineEvent::StageCompleted { output: risk_output.clone() });
            // Emit a dedicated structured event so consumers don't need to parse
            // raw JSON from the payload to display the risk assessment UI.
            {
                let p = &risk_output.payload;
                send!(PipelineEvent::RiskAssessed {
                    risk_level: p.get("risk_level").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
                    reason:     p.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    affected_areas: p.get("affected_areas")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default(),
                    breaking_change: p.get("breaking_change").and_then(|v| v.as_bool()).unwrap_or(false),
                    security_implications: p.get("security_implications").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    cr_focus: p.get("cr_focus").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    risk_unavailable: p.get("risk_unavailable").and_then(|v| v.as_bool()).unwrap_or(false),
                });
            }

            // ── Stage 4: Review ──────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Reviewer });
            let stage_timer = SpanTimer::start();
            // Exclude stale session-state fields (last_scope / last_plan from a prior run)
            // to prevent the Reviewer from accidentally applying wrong success criteria.
            // Pass effective_request + persistent_summary for user-intent context.
            let combined = serde_json::json!({
                "effective_request":  effective_prompt,
                "session_summary":    request_context.session_state.persistent_summary,
                "scope":              scoped.payload,
                "plan":               plan.payload,
                "code_changes":       code_output.payload,
                "risk_assessment":    risk_output.payload,
            });
            let context_for_reviewer = serde_json::to_string(&combined)?;
            let review_result = reviewer.execute(&context_for_reviewer).await;
            let review_tokens = review_result.as_ref().ok().and_then(|o| o.tokens);
            if let Some(t) = review_tokens { pipeline_tokens.add(t); }
            // Reviewer degrades gracefully on LLM/network error (mirrors Risk behaviour).
            // A transient API timeout must not kill an already-completed execution.
            let review_span_status = match &review_result {
                Ok(_) => SpanStatus::Ok,
                Err(e) => SpanStatus::Error { message: e.to_string() },
            };
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Reviewer, attempt }, review_span_status, review_tokens));
            let review = review_result.unwrap_or_else(|e| {
                warn!(error = %e, "Reviewer agent failed (non-fatal, advisory unavailable)");
                AgentOutput {
                    role: AgentRole::Reviewer,
                    summary: format!("Review unavailable: {e}"),
                    payload: serde_json::json!({
                        "approved": false,
                        "criteria_met": false,
                        "issues": [format!("Review could not be completed: {e}")],
                        "security_concerns": [],
                        "recommendation": "Review agent encountered an error — please inspect the changes manually.",
                        "review_unavailable": true,
                    }),
                    success: false,
                    tokens: None,
                }
            });
            send!(PipelineEvent::StageCompleted { output: review.clone() });
            // Emit a structured event so consumers display the full review without
            // needing to parse raw JSON from the StageCompleted payload.
            {
                let p = &review.payload;
                let extract_strings = |key: &str| -> Vec<String> {
                    p.get(key)
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default()
                };
                send!(PipelineEvent::ReviewCompleted {
                    approved:          p.get("approved").and_then(|v| v.as_bool()).unwrap_or(false),
                    criteria_met:      p.get("criteria_met").and_then(|v| v.as_bool()).unwrap_or(false),
                    issues:            extract_strings("issues"),
                    security_concerns: extract_strings("security_concerns"),
                    recommendation:    p.get("recommendation")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&review.summary)
                                        .to_string(),
                });
            }

            info!(attempt, "Pipeline converged successfully");
            // Persist a conversation turn so history is available on the next request.
            // Uses the Reviewer's summary as the response_summary — it is the most
            // complete synthesis of what the pipeline accomplished.
            self.persist_memory_patch(
                &request_context,
                &mut session_memory,
                SessionMemoryPatch {
                    new_conversation_turn: Some(ConversationTurn::new(
                        effective_prompt.as_str(),
                        review.summary.as_str(),
                    )),
                    last_risk_level: risk_output.payload
                        .get("risk_level")
                        .and_then(|v| v.as_str())
                        .filter(|&s| matches!(s, "low" | "medium" | "high"))
                        .map(str::to_string),
                    ..SessionMemoryPatch::default()
                },
            )
            .await?;
            // Pipeline succeeded — spawn the Compactor sub-agent as a fire-and-forget
            // background task. The Compactor re-reads the session, checks the token
            // budget, and runs an LLM summarisation pass if needed. The caller receives
            // the pipeline result immediately without waiting for compaction.
            if let (Some(store), Some(session_id), Some(memory)) = (
                self.memory_store.as_ref(),
                request_context.session_id.as_deref(),
                session_memory.as_ref(),
            ) {
                let agent    = CompactorAgent { llm: Arc::clone(&self.llm), store: Arc::clone(store) };
                let sid      = session_id.to_string();
                let snapshot = memory.clone();
                // DESIGN-2: compact() returns () and logs internally; spawn fire-and-forget.
                tokio::spawn(async move { agent.compact(&sid, snapshot).await });
            }
            result = Ok(vec![judge_output.clone(), scoped.clone(), plan, code_output, risk_output, review]);
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
            SpanKind::Pipeline { prompt: prompt.clone() },
            pipeline_status,
            Some(pipeline_tokens),
        ));
        obs.flush();

        result
    }
}

impl Controller {
    /// Run a single built-in sub-agent directly, bypassing the full pipeline.
    ///
    /// Events emitted:
    /// * [`PipelineEvent::StageStarted`] — before the agent executes.
    /// * [`PipelineEvent::StageCompleted`] — on success.
    /// * [`PipelineEvent::PipelineFailed`] — on error.
    ///
    /// For [`BuiltinAgentKind::Compactor`] the `task` argument is ignored; the
    /// agent reads from the session store directly using `session_id`.
    pub async fn run_single_agent(
        &self,
        kind: BuiltinAgentKind,
        task: &str,
        session_id: Option<&str>,
        tx: Option<mpsc::Sender<PipelineEvent>>,
    ) -> Result<AgentOutput, AgentError> {
        macro_rules! send {
            ($event:expr) => {
                if let Some(ref tx) = tx {
                    let _ = tx.send($event).await;
                }
            };
        }

        match kind {
            BuiltinAgentKind::Scoper => {
                send!(PipelineEvent::StageStarted { role: AgentRole::Scoper });
                let obs = ObsCtx::new_run(Arc::clone(&self.obs_sink));
                let guard = Arc::new(self.guard_template.build_scoper());
                let sensor_registry = Arc::new(crate::tools::ToolRegistry::with_sensors());
                let scoper = LlmScoperAgent {
                    llm: Arc::clone(&self.llm),
                    registry: sensor_registry,
                    guard,
                    obs,
                };
                match scoper.execute(task).await {
                    Ok(output) => {
                        send!(PipelineEvent::StageCompleted { output: output.clone() });
                        Ok(output)
                    }
                    Err(e) => {
                        send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                        Err(e)
                    }
                }
            }

            BuiltinAgentKind::Compactor => {
                // ── Sync precondition checks (before any async work) ──────────
                let store = match &self.memory_store {
                    Some(s) => Arc::clone(s),
                    None => {
                        let msg = "Compactor requires an attached session store \
                                   (run `harnesscode` from a project directory)."
                            .to_string();
                        send!(PipelineEvent::PipelineFailed { error: msg.clone() });
                        return Err(AgentError::Pipeline(msg));
                    }
                };
                let sid = match session_id {
                    Some(id) => id.to_string(),
                    None => {
                        let msg = "Compactor requires a session id.".to_string();
                        send!(PipelineEvent::PipelineFailed { error: msg.clone() });
                        return Err(AgentError::Pipeline(msg));
                    }
                };

                // Emit StageStarted before I/O so the spinner appears immediately.
                send!(PipelineEvent::StageStarted { role: AgentRole::Compactor });

                // ── Load snapshot ────────────────────────────────────────────
                let snapshot = match store.get_session(&sid).await? {
                    Some(mem) => mem,
                    // No persisted session yet → nothing to compact.
                    None => {
                        let output = AgentOutput {
                            role: AgentRole::Compactor,
                            summary: "No conversation history to compact.".to_string(),
                            payload: serde_json::Value::Null,
                            success: true,
                            tokens: None,
                        };
                        send!(PipelineEvent::StageCompleted { output: output.clone() });
                        return Ok(output);
                    }
                };

                // ── Skip if below the compaction threshold ───────────────────
                if !crate::memory::needs_compaction(&snapshot) {
                    let output = AgentOutput {
                        role: AgentRole::Compactor,
                        summary: "Session is below the compaction threshold — nothing to do."
                            .to_string(),
                        payload: serde_json::Value::Null,
                        success: true,
                        tokens: None,
                    };
                    send!(PipelineEvent::StageCompleted { output: output.clone() });
                    return Ok(output);
                }

                // ── Run the LLM compaction ────────────────────────────────────
                let compactor = CompactorAgent {
                    llm: Arc::clone(&self.llm),
                    store,
                };
                compactor.compact(&sid, snapshot).await;

                let output = AgentOutput {
                    role: AgentRole::Compactor,
                    summary: "Session memory compacted.".to_string(),
                    payload: serde_json::Value::Null,
                    success: true,
                    tokens: None,
                };
                send!(PipelineEvent::StageCompleted { output: output.clone() });
                Ok(output)
            }
        }
    }

    async fn load_session_memory(
        &self,
        request_context: &mut RequestContext,
    ) -> Result<Option<SessionMemory>, AgentError> {
        let Some(store) = self.memory_store.as_ref() else {
            return Ok(None);
        };
        let Some(session_id) = request_context.session_id.as_deref() else {
            return Ok(None);
        };

        let memory = store
            .get_session(session_id)
            .await?
            .unwrap_or_else(|| SessionMemory::new(session_id.to_string(), Some(truncate_title(&request_context.current_request))));
        merge_memory_into_request_context(request_context, &memory);
        Ok(Some(memory))
    }

    async fn persist_memory_patch(
        &self,
        request_context: &RequestContext,
        session_memory: &mut Option<SessionMemory>,
        mut patch: SessionMemoryPatch,
    ) -> Result<(), AgentError> {
        let (Some(store), Some(session_id)) = (self.memory_store.as_ref(), request_context.session_id.as_deref()) else {
            return Ok(());
        };

        if patch.title.is_none() {
            patch.title = Some(truncate_title(&request_context.current_request));
        }
        // Apply the patch to the in-memory cached copy instead of re-reading from disk.
        // This avoids one disk read per stage while still writing the updated state.
        let sid = session_id.to_string();
        let title_fallback = patch.title.clone();
        let memory = session_memory.get_or_insert_with(|| {
            SessionMemory::new(sid, title_fallback)
        });
        apply_patch(memory, patch);
        store.save_session(memory).await?;
        Ok(())
    }
}

fn merge_memory_into_request_context(request_context: &mut RequestContext, memory: &SessionMemory) {
    if request_context.session_state.execution_summary.is_none() {
        request_context.session_state.execution_summary = memory.execution_summary.clone();
    }
    if request_context.session_state.persistent_summary.is_none() {
        request_context.session_state.persistent_summary = memory.persistent_summary.clone();
    }
    merge_unique_strings(
        &mut request_context.session_state.clarified_facts,
        memory.clarified_facts.clone(),
    );
    merge_unique_strings(
        &mut request_context.session_state.known_relevant_files,
        memory.known_relevant_files.clone(),
    );
    if request_context.session_state.open_questions.is_empty() {
        request_context.session_state.open_questions = memory.open_questions.clone();
    }
    if request_context.session_state.last_scope.is_none() {
        request_context.session_state.last_scope = memory.last_scope.clone();
    }
    if request_context.session_state.last_plan.is_none() {
        request_context.session_state.last_plan = memory.last_plan.clone();
    }

    // Build recent_messages and conversation_summary from persisted conversation turns.
    // This ensures history is available to all agents regardless of the calling client
    // (desktop, CLI, or test), and survives page reloads and process restarts.
    //
    // The two concerns are kept independent: recent_messages is only populated when
    // the caller hasn't pre-filled it, but conversation_summary is always derived
    // from compacted_summary (or older turns) regardless — so LLM-generated context
    // is never silently dropped for callers that supply their own recent_messages.
    if request_context.recent_messages.is_empty() && !memory.conversation_turns.is_empty() {
        let turns = &memory.conversation_turns;
        let window_start = turns.len().saturating_sub(RECENT_TURNS_WINDOW);

        // The most-recent turns go into recent_messages as verbatim user/assistant pairs.
        for turn in &turns[window_start..] {
            request_context.recent_messages.push(ConversationMessage {
                role: ConversationRole::User,
                content: turn.request.clone(),
            });
            request_context.recent_messages.push(ConversationMessage {
                role: ConversationRole::Assistant,
                content: turn.response_summary.clone(),
            });
        }
    }

    // Older turns: use the LLM-generated compacted_summary if available;
    // fall back to raw join only before the first compaction pass.
    // Done unconditionally so callers that supply recent_messages still get the summary.
    if request_context.conversation_summary.is_none() {
        if let Some(ref s) = memory.compacted_summary {
            request_context.conversation_summary = Some(s.clone());
        } else if !memory.conversation_turns.is_empty() {
            let turns = &memory.conversation_turns;
            let window_start = turns.len().saturating_sub(RECENT_TURNS_WINDOW);
            let older = &turns[..window_start];
            if !older.is_empty() {
                let summary = older
                    .iter()
                    .map(|t| format!("User: {}\nAssistant: {}", t.request.trim(), t.response_summary.trim()))
                    .collect::<Vec<_>>()
                    .join("\n---\n");
                request_context.conversation_summary = Some(summary);
            }
        }
    }
}

fn merge_unique_strings(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !value.trim().is_empty() && !target.iter().any(|existing| existing == &value) {
            target.push(value);
        }
    }
}

fn truncate_title(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= 60 {
        return trimmed.to_string();
    }
    trimmed.chars().take(59).collect::<String>() + "…"
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

            let content = if sys.contains("request-judging agent") {
                r#"{"route":"scoper","route_reason_code":"needs_repository_grounding","ask_user_clarification":false,"effective_request":"Add a hello world function","decision_factors":{"goal_is_concrete":true,"constraints_are_stable":true,"history_resolves_references":false,"repository_grounding_needed":true,"prior_scope_can_be_reused":false},"skip_scoper_criteria_met":[],"missing_information":[],"clarifying_questions":[],"confidence":"high"}"#
            } else if sys.contains("planning agent") {
                r#"{"phases":[{"phase_id":1,"title":"Implement hello world","objective":"Add the function","steps":["Analyse codebase","Generate diff","Run tests"],"affected_files":["src/main.rs"],"success_criteria":"tests pass","complexity":"low"}],"global_success_criteria":"all phases done","complexity":"low"}"#
            } else if sys.contains("architect and security engineer") {
                r#"{"risk_level":"low","reason":"no significant risk detected","affected_areas":[],"breaking_change":false,"security_implications":"","cr_focus":"standard code review"}"#
            } else if sys.contains("problem-framing agent") {
                r#"{"task_type":"feature","objective":"Add a hello world function","problem_statement":"Introduce a simple hello world function in the appropriate module","in_scope":["Rust source change"],"out_of_scope":["CLI redesign"],"constraints":[],"assumptions":[],"unknowns":[],"relevant_files":["src/main.rs"],"success_criteria":["A hello world function exists"],"needs_user_clarification":false,"clarifying_questions":[],"confidence":"high"}"#
            } else if sys.contains("expert software engineer") {
                r#"{"phase_id":1,"diff":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-// TODO\n+// Done","files_changed":1,"explanation":"Replaced placeholder","language":"rust","success_criteria_met":true,"affected_files_delta_reason":""}"#
            } else {
                r#"{"approved":true,"criteria_met":true,"issues":[],"security_concerns":[],"recommendation":"LGTM"}"#
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
        assert_eq!(outputs.len(), 6);
        assert!(outputs.iter().all(|o| o.success));
    }

    /// Stateful mock that drives a two-phase plan: Planner emits two phases,
    /// and the Conductor mock returns the correct `phase_id` on each of its calls.
    struct MultiPhaseMockLlmProvider {
        conductor_call: std::sync::atomic::AtomicUsize,
    }

    impl MultiPhaseMockLlmProvider {
        fn new() -> Self {
            Self { conductor_call: std::sync::atomic::AtomicUsize::new(0) }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MultiPhaseMockLlmProvider {
        fn provider_name(&self) -> &str { "mock" }
        fn model_name(&self) -> &str { "mock-model" }

        async fn complete(&self, messages: &[LlmMessage]) -> Result<LlmResponse, LlmError> {
            let sys = messages
                .iter()
                .find(|m| m.role == crate::llm::MessageRole::System)
                .map(|m| m.content.as_str())
                .unwrap_or("");

            let content = if sys.contains("request-judging agent") {
                r#"{"route":"scoper","route_reason_code":"needs_repository_grounding","ask_user_clarification":false,"effective_request":"Refactor two modules","decision_factors":{"goal_is_concrete":true,"constraints_are_stable":true,"history_resolves_references":false,"repository_grounding_needed":true,"prior_scope_can_be_reused":false},"skip_scoper_criteria_met":[],"missing_information":[],"clarifying_questions":[],"confidence":"high"}"#
            } else if sys.contains("planning agent") {
                r#"{"phases":[{"phase_id":1,"title":"Phase one","objective":"Do part one","steps":["Step A"],"affected_files":["src/main.rs"],"success_criteria":"part one done","complexity":"low"},{"phase_id":2,"title":"Phase two","objective":"Do part two","steps":["Step B"],"affected_files":["src/lib.rs"],"success_criteria":"part two done","complexity":"low"}],"global_success_criteria":"all done","complexity":"low"}"#
            } else if sys.contains("architect and security engineer") {
                r#"{"risk_level":"low","reason":"no significant risk","affected_areas":[],"breaking_change":false,"security_implications":"","cr_focus":"standard code review"}"#
            } else if sys.contains("problem-framing agent") {
                r#"{"task_type":"feature","objective":"Refactor two modules","problem_statement":"Refactor two modules","in_scope":["Rust"],"out_of_scope":[],"constraints":[],"assumptions":[],"unknowns":[],"relevant_files":["src/main.rs","src/lib.rs"],"success_criteria":["Done"],"needs_user_clarification":false,"clarifying_questions":[],"confidence":"high"}"#
            } else if sys.contains("expert software engineer") {
                let call = self.conductor_call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    r#"{"phase_id":1,"diff":"--- a/src/main.rs\n+++ b/src/main.rs","files_changed":1,"explanation":"Phase one done","language":"rust","success_criteria_met":true,"affected_files_delta_reason":""}"#
                } else {
                    r#"{"phase_id":2,"diff":"--- a/src/lib.rs\n+++ b/src/lib.rs","files_changed":1,"explanation":"Phase two done","language":"rust","success_criteria_met":true,"affected_files_delta_reason":""}"#
                }
            } else {
                r#"{"approved":true,"criteria_met":true,"issues":[],"security_concerns":[],"recommendation":"LGTM"}"#
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
    async fn test_controller_runs_two_phase_plan() {
        let controller = Controller::new(3, Arc::new(MultiPhaseMockLlmProvider::new()));
        let outputs = controller
            .run("refactor two modules")
            .await
            .unwrap();
        eprintln!("Outputs count: {}", outputs.len());
        for (i, out) in outputs.iter().enumerate() {
            eprintln!("  [{}] role={:?}, success={}", i, out.role, out.success);
        }
        assert_eq!(outputs.len(), 6, "Expected 6 agent outputs");
        assert!(outputs.iter().all(|o| o.success), "All agents should succeed");
        // The Conductor stage output should contain both phase_outputs in order.
        let conductor_out = outputs.iter().find(|o| o.role == AgentRole::Conductor)
            .expect("Conductor agent should be in outputs");
        let phase_outputs = conductor_out.payload["phase_outputs"].as_array()
            .expect("Conductor should have phase_outputs array");
        assert_eq!(phase_outputs.len(), 2, "Should have 2 phase outputs");
        assert_eq!(phase_outputs[0]["phase_id"], 1);
        assert_eq!(phase_outputs[1]["phase_id"], 2);
        assert_eq!(phase_outputs[0]["explanation"], "Phase one done");
        assert_eq!(phase_outputs[1]["explanation"], "Phase two done");
    }

    #[tokio::test]
    async fn test_agent_roles() {
        let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider);
        let guard = Arc::new(ExecutionGuard::default());

        assert_eq!(JudgeAgent { llm: Arc::clone(&llm) }.llm.model_name(), "mock-model");
        assert_eq!(
            LlmScoperAgent {
                llm:      Arc::clone(&llm),
                registry: Arc::new(crate::tools::ToolRegistry::with_sensors()),
                guard:    Arc::clone(&guard),
                obs:      crate::observability::ObsCtx::noop(),
            }.role(),
            AgentRole::Scoper,
        );
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
            LlmConductorAgent {
                llm: Arc::clone(&llm),
                registry: Arc::new(crate::tools::ToolRegistry::empty()),
                guard,
                obs: crate::observability::ObsCtx::noop(),
                drift: None,
            }
            .role(),
            AgentRole::Conductor
        );
        assert_eq!(LlmRiskAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Risk);
        assert_eq!(LlmReviewerAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Reviewer);
    }

    #[test]
    fn request_context_detects_missing_history() {
        let vague = RequestContext::from_prompt("优化一下登录流程");
        assert!(!vague.has_meaningful_context());
    }

    #[test]
    fn history_aware_requests_do_not_force_scoper() {
        let request_context = RequestContext {
            session_id: Some("default".into()),
            current_request: "继续按刚才的方案改".into(),
            conversation_summary: Some("User already approved the earlier controller refactor.".into()),
            recent_messages: vec![crate::controller::ConversationMessage {
                role: crate::controller::ConversationRole::User,
                content: "Use the same event contract as above".into(),
            }],
            session_state: crate::controller::SessionState {
                execution_summary: Some("Planner produced a concrete event wiring plan".into()),
                last_scope: None,
                last_plan: None,
                persistent_summary: Some("User already approved the controller refactor direction.".into()),
                clarified_facts: vec!["Reuse the same event contract as above".into()],
                known_relevant_files: vec!["apps/desktop/src-tauri/src/lib.rs".into()],
                open_questions: Vec::new(),
            },
        };

        assert!(request_context.has_meaningful_context());
    }
}
