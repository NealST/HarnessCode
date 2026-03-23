//! The cybernetic Controller — schedules agents and enforces pipeline-level rules.
//!
//! The Controller is the "safety harness" around the multi-agent pipeline:
//! * Drives the Judge → Scoper → Planner → Conductor → Risk → Reviewer sequence.
//! * Enforces retry limits and step budgets.
//! * Constructs a fresh [`ExecutionGuard`] for each attempt so resource budgets
//!   reset cleanly between retries.
//! * Handles agent errors with retry strategies.

use super::events::PipelineEvent;
use super::guardrails::ExecutionGuard;
use super::interaction::{ClarificationCallback, ClarificationRequest, ClarificationResolution};
use super::request_context::RequestContext;
use crate::agents::{
    compactor::CompactorAgent,
    drift_judge::{DriftCallback, DriftConfig, DriftJudgeAgent, DriftParams},
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
    /// No step budget — the Planner only has sensor tools and cannot modify
    /// anything, so the worst case is extra token spend (bounded by the LLM
    /// provider's own limits).  Only dedup applies.
    fn build_planner(&self) -> ExecutionGuard {
        ExecutionGuard::unlimited()
    }

    /// Build a guard for the Scoper's read-only framing loop.
    fn build_scoper(&self) -> ExecutionGuard {
        ExecutionGuard::new(self.max_tool_turns.min(8))
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
    AgentOutput {
        role: AgentRole::Scoper,
        summary: "Problem framed from a well-specified request".to_string(),
        payload: serde_json::json!({
            "task_type": "other",
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
        self.run_with_request_context(&RequestContext::from_prompt(prompt), None, None, None)
            .await
    }

    /// Run the pipeline for an already-assembled request context.
    pub async fn run_with_context(
        &self,
        request_context: &RequestContext,
    ) -> Result<Vec<AgentOutput>, AgentError> {
        self.run_with_request_context(request_context, None, None, None).await
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
        self.run_with_request_context(&RequestContext::from_prompt(prompt), tx, drift_callback, None)
            .await
    }

    /// Run the pipeline using a full request context, streaming [`PipelineEvent`]s to `tx`.
    pub async fn run_with_request_context(
        &self,
        request_context: &RequestContext,
        tx: Option<mpsc::Sender<PipelineEvent>>,
        drift_callback: Option<DriftCallback>,
        clarification_callback: Option<ClarificationCallback>,
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
        let result: Result<Vec<AgentOutput>, AgentError>;

        // The effective prompt may be reinforced on a drift-triggered restart.
        // `original_prompt` is fixed for the lifetime of this call so the judge
        // always receives the true user intent, never the reinforced variant.
        let original_prompt = prompt.clone();
        let mut effective_prompt = original_prompt.clone();

        // Accumulate total tokens across all agents across all attempts.
        let mut pipeline_tokens = TokenUsage::default();

        let sensor_registry = Arc::new(crate::tools::ToolRegistry::with_sensors());
        let judge = JudgeAgent {
            llm: Arc::clone(&self.llm),
        };
        let scoper_obs = obs.child(pipeline_span_id);
        let scoper_guard = Arc::new(self.guard_template.build_scoper());
        let scoper = LlmScoperAgent {
            llm: Arc::clone(&self.llm),
            registry: Arc::clone(&sensor_registry),
            guard: scoper_guard,
            obs: scoper_obs,
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
                                    open_questions: request_context.session_state.open_questions.clone(),
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
        // Keep effective_prompt in sync with what the Judge resolved so the
        // Planner always receives the fully-interpreted request.
        effective_prompt = effective_request_override.clone();

        // ── Stage 1: Scoping ──────────────────────────────────────────────
        if should_run_scoper {
            send!(PipelineEvent::StageStarted { role: AgentRole::Scoper });
        }
        let stage_timer = SpanTimer::start();
        let mut scope_result = if should_run_scoper {
            Some(scoper.execute(&scoper_input).await)
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
        obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Scoper, attempt: 1 }, scope_span_status, scope_tokens));
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
                send!(PipelineEvent::PipelineFailed { error: format!("Scoper failed: {}", o.summary) });
                result = Err(AgentError::ExecutionFailed { role: AgentRole::Scoper, message: o.summary.clone() });
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
                                open_questions: request_context.session_state.open_questions.clone(),
                                ..SessionMemoryPatch::default()
                            },
                        )
                        .await?;
                        let retry_timer = SpanTimer::start();
                        scope_result = Some(scoper.execute(&serde_json::json!({
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
                            Some(Ok(output)) => output,
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
                open_questions: payload_strings(&scoped.payload, "clarifying_questions"),
                last_scope: Some(scoped.payload.clone()),
                ..SessionMemoryPatch::default()
            },
        )
        .await?;
        send!(PipelineEvent::StageCompleted { output: scoped.clone() });
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

        loop {
            attempt += 1;
            info!(attempt, "Starting pipeline run");

            let guard = Arc::new(self.guard_template.build());

            // Conductor's ObsCtx is a child of the pipeline span so its ToolTurn/
            // ToolCall children nest correctly in the span tree.
            let conductor_obs = obs.child(pipeline_span_id);

            // Construct agents.
            let planner_guard   = Arc::new(self.guard_template.build_planner());
            let planner_obs     = obs.child(pipeline_span_id);
            let planner = LlmPlannerAgent {
                llm:      Arc::clone(&self.llm),
                registry: Arc::clone(&sensor_registry),
                guard:    planner_guard,
                obs:      planner_obs,
            };
            // Build drift params for the conductor if a judge and callback are available.
            let conductor_drift = self.drift_judge.as_ref()
                .zip(drift_callback.as_ref())
                .map(|(judge, callback)| DriftParams {
                    judge: Arc::clone(judge),
                    config: self.drift_config.clone(),
                    // Always use the immutable original prompt so the judge
                    // sees the user's real intent even after a restart.
                    original_prompt: original_prompt.clone(),
                    callback: Arc::clone(callback),
                });
            let conductor = LlmConductorAgent { llm: Arc::clone(&self.llm), registry: Arc::clone(&self.registry), guard: Arc::clone(&guard), obs: conductor_obs, drift: conductor_drift };
            let risk_agent = LlmRiskAgent     { llm: Arc::clone(&self.llm) };
            let reviewer   = LlmReviewerAgent { llm: Arc::clone(&self.llm) };

            // ── Stage 1: Planning ────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Planner });
            let stage_timer = SpanTimer::start();
            let planner_input = serde_json::json!({
                "request_context": request_context,
                "user_request": original_prompt,
                    "effective_request": effective_prompt,
                "problem_frame": scoped.payload,
            }).to_string();
            let plan_result = planner.execute(&planner_input).await;
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
            self.persist_memory_patch(
                &request_context,
                &mut session_memory,
                SessionMemoryPatch {
                    execution_summary: Some(plan.summary.clone()),
                    known_relevant_files: payload_strings(&plan.payload, "affected_files"),
                    last_plan: Some(plan.payload.clone()),
                    ..SessionMemoryPatch::default()
                },
            )
            .await?;
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
            // ── Stage 2: Execution (Conductor) ────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Conductor });
            let stage_timer = SpanTimer::start();
            let context_for_conductor = serde_json::json!({
                "request_context": request_context,
                "problem_frame": scoped.payload,
                "plan": plan.payload,
            }).to_string();
            let code_result = conductor.execute(&context_for_conductor).await;
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
            obs.record(stage_timer.finish(obs.run_id, Some(pipeline_span_id), SpanKind::Stage { role: AgentRole::Conductor, attempt }, code_span_status, code_tokens));
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
                    warn!(error = %e, "Conductor failed");
                    maybe_send_network_error!(e, AgentRole::Conductor);
                    send!(PipelineEvent::PipelineFailed { error: e.to_string() });
                    if attempt >= self.max_retries { result = Err(AgentError::MaxRetriesExceeded(self.max_retries)); break; }
                    continue;
                }
                Ok(o) if !o.success => {
                    warn!(role = %o.role, "Conductor reported failure");
                    send!(PipelineEvent::PipelineFailed { error: format!("Conductor failed: {}", o.summary) });
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
                "request_context": request_context,
                "scope": scoped.payload,
                "plan": plan.payload,
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
                r#"{"steps":["Analyse codebase","Generate diff","Run tests"],"affected_files":["src/main.rs"],"success_criteria":"tests pass","complexity":"low"}"#
            } else if sys.contains("architect and security engineer") {
                r#"{"risk_level":"low","reason":"no significant risk detected","affected_areas":[],"breaking_change":false,"security_implications":"","cr_focus":"standard code review"}"#
            } else if sys.contains("problem-framing agent") {
                r#"{"task_type":"feature","objective":"Add a hello world function","problem_statement":"Introduce a simple hello world function in the appropriate module","in_scope":["Rust source change"],"out_of_scope":["CLI redesign"],"constraints":[],"assumptions":[],"unknowns":[],"relevant_files":["src/main.rs"],"success_criteria":["A hello world function exists"],"needs_user_clarification":false,"clarifying_questions":[],"confidence":"high"}"#
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
        assert_eq!(outputs.len(), 6);
        assert!(outputs.iter().all(|o| o.success));
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
