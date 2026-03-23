//! # Multi-Agent Orchestration
//!
//! This module defines the core abstractions for HarnessCode's multi-agent pipeline:
//!
//! * [`AgentRole`] — enum identifying a specific role in the pipeline.
//! * [`Agent`] — async trait that every agent must implement.
//! * [`AgentOutput`] — the result produced by a single agent execution.
//! * [`Controller`] — cybernetic feedback loop that drives the pipeline to convergence.

use crate::llm::{LlmCompletion, LlmMessage, LlmProvider};
use crate::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};

// ──────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────

/// Errors that can occur during agent execution or controller orchestration.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent '{role}' failed: {message}")]
    ExecutionFailed { role: AgentRole, message: String },

    #[error("maximum retry limit ({0}) reached without converging")]
    MaxRetriesExceeded(usize),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("LLM provider error: {0}")]
    Provider(#[from] crate::llm::LlmError),
}

// ──────────────────────────────────────────────
// Agent role
// ──────────────────────────────────────────────

/// Identifies which role in the pipeline an agent fulfils.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    /// Breaks the user's goal into discrete, verifiable steps.
    Planner,
    /// Executes the plan — applies changes, runs commands, and produces a diff summary.
    Conductor,
    /// Analyses the code diff for semantic risk and tags it for CR awareness.
    Risk,
    /// Validates the output, runs tests, and decides pass/fail.
    Reviewer,
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentRole::Planner => write!(f, "Planner"),
            AgentRole::Conductor => write!(f, "Conductor"),
            AgentRole::Risk => write!(f, "Risk"),
            AgentRole::Reviewer => write!(f, "Reviewer"),
        }
    }
}

// ──────────────────────────────────────────────
// Agent output
// ──────────────────────────────────────────────

/// The structured output produced by an [`Agent`] after processing a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// Which agent produced this output.
    pub role: AgentRole,
    /// Human-readable summary of what the agent did.
    pub summary: String,
    /// Arbitrary structured payload (e.g. a diff, a plan, a review report).
    pub payload: serde_json::Value,
    /// Whether the agent considers its sub-task complete.
    pub success: bool,
}

// ──────────────────────────────────────────────
// Pipeline progress events
// ──────────────────────────────────────────────

/// Events emitted by [`Controller::run_with_progress`] as the pipeline runs.
///
/// Consumers can display real-time status (e.g. spinners) by processing these
/// events on the receiving end of a [`tokio::sync::mpsc`] channel.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// An agent stage has started.  Callers should show a spinner / "thinking" indicator.
    StageStarted { role: AgentRole },
    /// An agent stage completed successfully.  `output` contains the real summary.
    StageCompleted { output: AgentOutput },
    /// The pipeline failed (either an agent error or max retries exceeded).
    PipelineFailed { error: String },
}

// ──────────────────────────────────────────────
// Agent trait
// ──────────────────────────────────────────────

/// Every participant in the HarnessCode pipeline must implement this trait.
///
/// Agents are intentionally stateless between calls; all state is passed in
/// via `context` and returned via [`AgentOutput`].
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// Returns the role this agent fulfils in the pipeline.
    fn role(&self) -> AgentRole;

    /// Execute the agent's task given the current `context` string.
    ///
    /// Returns an [`AgentOutput`] describing the result, or an [`AgentError`]
    /// if execution failed irrecoverably.
    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError>;
}

// ──────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────

/// Try to parse `text` as JSON; if it fails, return `{"raw": "<text>"}`.
fn parse_json_or_wrap(text: &str) -> serde_json::Value {
    serde_json::from_str(text)
        .unwrap_or_else(|_| serde_json::json!({ "raw": text }))
}

// ──────────────────────────────────────────────
// LLM-backed agents
// ──────────────────────────────────────────────

// ── Planner ────────────────────────────────────

const PLANNER_SYSTEM: &str = "\
You are a senior software engineer acting as a technical planning agent for HarnessCode, a safe AI coding assistant.
Your job is to decompose a coding task into a precise, executable plan.

Respond ONLY with a valid JSON object in this exact format:
{
  \"steps\": [\"step 1\", \"step 2\"],
  \"affected_files\": [\"src/file.rs\"],
  \"success_criteria\": \"all tests pass and the feature works as described\",
  \"complexity\": \"low\"
}

Fields:
- steps: ordered list of atomic, concrete actions
- affected_files: list of file paths that will be created or modified
- success_criteria: what done looks like
- complexity: one of low | medium | high

Do not include any text outside the JSON object.";

/// Planner agent backed by an LLM. Decomposes the user's task into steps.
pub struct LlmPlannerAgent {
    pub llm: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl Agent for LlmPlannerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Planner
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Planner, "Analysing task and building execution plan");

        let messages = vec![
            LlmMessage::system(PLANNER_SYSTEM),
            LlmMessage::user(format!("Task: {context}")),
        ];

        let response = self.llm.complete(&messages).await?;
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
        })
    }
}

// ── Conductor ────────────────────────────────────────────

const CONDUCTOR_SYSTEM: &str = "\
You are an expert software engineer working on a real codebase via HarnessCode.
You have access to tools to read and modify files. Use them to implement the task.

## Workflow
1. Use read_file / list_directory / search_files to understand the current code.
2. Use write_file or apply_diff to apply your changes.
3. Use run_command to verify (e.g. `cargo build`, `cargo test`).
4. Once all changes are applied and verified, respond with a JSON summary.

## Final response format
When you are done making changes, respond ONLY with a valid JSON object:
{
  \"diff\": \"--- a/src/main.rs\\n+++ b/src/main.rs\\n@@ -1 +1 @@\\n-// TODO\\n+// Done\",
  \"files_changed\": 1,
  \"explanation\": \"Replaced placeholder comment with implementation\",
  \"language\": \"rust\"
}

Fields:
- diff: unified diff summarising ALL changes you made (--- a/path / +++ b/path)
- files_changed: number of files you modified or created
- explanation: concise description of what changed and why
- language: primary programming language used

Do not include any text outside the JSON object in your final response.";

/// Conductor agent backed by an LLM. Executes the plan from the Planner's output.
pub struct LlmConductorAgent {
    pub llm: Arc<dyn LlmProvider>,
    pub registry: Arc<ToolRegistry>,
}

#[async_trait::async_trait]
impl Agent for LlmConductorAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Conductor
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Conductor, "Executing plan");

        let messages = vec![
            LlmMessage::system(CONDUCTOR_SYSTEM),
            LlmMessage::user(format!("Execution plan:\n{context}")),
        ];

        let text = run_tool_loop(&self.llm, messages, &self.registry).await?;
        let payload = parse_json_or_wrap(&text);

        let summary = payload
            .get("explanation")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Code changes generated".to_string());

        Ok(AgentOutput {
            role: AgentRole::Conductor,
            summary,
            payload,
            success: true,
        })
    }
}

// ── Risk ─────────────────────────────────────────

const RISK_SYSTEM: &str = "\
You are a senior software architect and security engineer at HarnessCode. \
Analyse the provided code diff to assess its risk level for a production system. \
Focus on semantic impact — what the change actually does — not just file names.

Respond ONLY with a valid JSON object in this exact format:
{
  \"risk_level\": \"low\",
  \"reason\": \"brief explanation of why this risk level was assigned\",
  \"affected_areas\": [\"authentication\"],
  \"breaking_change\": false,
  \"security_implications\": \"\",
  \"cr_focus\": \"what reviewers should pay most attention to\"
}

Fields:
- risk_level: \"low\" | \"medium\" | \"high\"
- reason: concise explanation (1-2 sentences)
- affected_areas: list of system areas affected (empty array if none)
- breaking_change: true if this could break existing behaviour or API contracts
- security_implications: description of security impact, or empty string if none
- cr_focus: specific guidance for reviewers on what to examine carefully

Use \"high\" if the change modifies authentication/authorisation logic, cryptographic operations, \
database schemas, external API contracts, or introduces injection/data-leakage vectors.
Use \"medium\" if the change modifies configuration loading, environment handling, logging, \
error handling, dependency versions, or CI/CD pipeline code.
Use \"low\" for all other changes.
Do not include any text outside the JSON object.";

/// Risk agent backed by an LLM. Analyses the Coder's diff for semantic risk.
pub struct LlmRiskAgent {
    pub llm: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl Agent for LlmRiskAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Risk
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Risk, "Analysing code changes for semantic risk");

        let messages = vec![
            LlmMessage::system(RISK_SYSTEM),
            LlmMessage::user(format!("Code changes to assess:\n{context}")),
        ];

        let response = self.llm.complete(&messages).await?;
        let payload = parse_json_or_wrap(&response.content);

        let risk_level = payload
            .get("risk_level")
            .and_then(|v| v.as_str())
            .unwrap_or("low");

        let reason = payload
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no significant risk detected");

        let summary = format!("[{}] {}", risk_level.to_uppercase(), reason);

        Ok(AgentOutput {
            role: AgentRole::Risk,
            summary,
            payload,
            // Risk assessment is always informational — it never blocks the pipeline.
            success: true,
        })
    }
}

// ── Reviewer ───────────────────────────────────

const REVIEWER_SYSTEM: &str = "\
You are a senior code reviewer at HarnessCode specialising in security, correctness, and code quality.
Analyse the provided code changes and decide whether to approve them.

Respond ONLY with a valid JSON object in this exact format:
{
  \"approved\": true,
  \"issues\": [],
  \"security_concerns\": [],
  \"recommendation\": \"LGTM — code is correct and safe to merge\"
}

Fields:
- approved: true if changes are safe and correct, false if they must be revised
- issues: list of functional or quality problems (empty array if none)
- security_concerns: list of security problems such as injection, data leaks, etc. (empty array if none)
- recommendation: one-sentence human-readable verdict

Set approved to false if there are ANY critical issues or security concerns.
Do not include any text outside the JSON object.";

/// Reviewer agent backed by an LLM. Validates the Coder's output.
pub struct LlmReviewerAgent {
    pub llm: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl Agent for LlmReviewerAgent {
    fn role(&self) -> AgentRole {
        AgentRole::Reviewer
    }

    async fn execute(&self, context: &str) -> Result<AgentOutput, AgentError> {
        info!(role = %AgentRole::Reviewer, "Reviewing generated code changes");

        let messages = vec![
            LlmMessage::system(REVIEWER_SYSTEM),
            LlmMessage::user(format!("Code changes to review:\n{context}")),
        ];

        let response = self.llm.complete(&messages).await?;
        let payload = parse_json_or_wrap(&response.content);

        let approved = payload
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let recommendation = payload
            .get("recommendation")
            .and_then(|r| r.as_str())
            .unwrap_or(if approved {
                "Approved"
            } else {
                "Rejected — revisions required"
            });

        Ok(AgentOutput {
            role: AgentRole::Reviewer,
            summary: recommendation.to_string(),
            payload,
            success: approved,
        })
    }
}

// ──────────────────────────────────────────────
// Tool loop — agentic ReAct cycle
// ──────────────────────────────────────────────

/// Agentic tool loop (module-level free function).
///
/// Repeatedly calls the LLM with the current message history and executes any
/// tools it requests, until the model produces a final text answer.
///
/// This is the core of the *Harness Engineering* model: the LLM reasons and
/// selects tools; the registry executes them with guaranteed fidelity.
async fn run_tool_loop(
    llm: &Arc<dyn LlmProvider>,
    mut messages: Vec<LlmMessage>,
    registry: &ToolRegistry,
) -> Result<String, AgentError> {
    const MAX_TOOL_TURNS: usize = 20;
    let mut turns = 0;

    loop {
        turns += 1;
        if turns > MAX_TOOL_TURNS {
            return Err(AgentError::ExecutionFailed {
                role: AgentRole::Planner, // placeholder — loop is agent-agnostic
                message: format!("Tool loop exceeded {} turns without finishing", MAX_TOOL_TURNS),
            });
        }

        match llm.complete_with_tools(&messages, &registry.defs()).await? {
            LlmCompletion::Done { text, .. } => return Ok(text),
            LlmCompletion::NeedTools { calls, .. } => {
                // Record the assistant's tool-call turn.
                messages.push(LlmMessage::assistant_tool_calls(calls.clone()));

                // Execute all requested tools concurrently.
                let results = futures::future::join_all(
                    calls.iter().map(|c| registry.dispatch(c))
                ).await;

                // Feed each result back as a tool-result message.
                for (call, result) in calls.iter().zip(results.iter()) {
                    messages.push(LlmMessage::tool_result(
                        &call.id,
                        &result.content,
                        result.is_error,
                    ));
                }
            }
        }
    }
}

// ──────────────────────────────────────────────
// Controller — cybernetic feedback loop
// ──────────────────────────────────────────────

/// The cybernetic controller that drives the multi-agent pipeline.
///
/// Implements a **TOTE** (Test-Operate-Test-Exit) loop:
/// 1. **Test** — Planner evaluates the current state vs. the desired goal.
/// 2. **Operate** — Coder applies changes (with tool calls if needed).
/// 3. **Test** — Reviewer checks the result against success criteria.
/// 4. **Exit** — Loop terminates on success or after `max_retries` failures.
pub struct Controller {
    /// Maximum number of full pipeline retries before giving up.
    pub max_retries: usize,
    /// The LLM backend shared by all agents in the pipeline.
    pub llm: Arc<dyn LlmProvider>,
    /// The tool registry available to agents during tool-use turns.
    pub registry: Arc<ToolRegistry>,
}

impl Controller {
    /// Create a new [`Controller`] with the given retry limit and LLM provider.
    /// Loads all builtin tools automatically.
    pub fn new(max_retries: usize, llm: Arc<dyn LlmProvider>) -> Self {
        Self {
            max_retries,
            llm,
            registry: Arc::new(ToolRegistry::with_builtins()),
        }
    }

    /// Create a [`Controller`] with a custom [`ToolRegistry`].
    pub fn new_with_tools(
        max_retries: usize,
        llm: Arc<dyn LlmProvider>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self { max_retries, llm, registry }
    }

    /// Run the full Planner → Coder → Reviewer pipeline for the given `prompt`.
    ///
    /// Returns a [`Vec`] of [`AgentOutput`] from each stage of the final
    /// successful (or last attempted) run.
    pub async fn run(&self, prompt: &str) -> Result<Vec<AgentOutput>, AgentError> {
        self.run_with_progress(prompt, None).await
    }

    /// Run the pipeline, optionally sending [`PipelineEvent`]s to `tx`.
    ///
    /// Each agent emits a [`PipelineEvent::StageStarted`] before it calls the
    /// LLM and a [`PipelineEvent::StageCompleted`] (or [`PipelineEvent::PipelineFailed`])
    /// on completion.  Dropped send errors are silently ignored so that callers
    /// can close the receiver early without aborting the pipeline.
    pub async fn run_with_progress(
        &self,
        prompt: &str,
        tx: Option<mpsc::Sender<PipelineEvent>>,
    ) -> Result<Vec<AgentOutput>, AgentError> {
        let planner = LlmPlannerAgent { llm: Arc::clone(&self.llm) };
        let conductor = LlmConductorAgent { llm: Arc::clone(&self.llm), registry: Arc::clone(&self.registry) };
        let risk_agent = LlmRiskAgent { llm: Arc::clone(&self.llm) };
        let reviewer = LlmReviewerAgent { llm: Arc::clone(&self.llm) };

        // Helper: fire an event, ignoring a closed receiver.
        macro_rules! send {
            ($event:expr) => {
                if let Some(ref tx) = tx {
                    let _ = tx.send($event).await;
                }
            };
        }

        let mut attempt = 0;

        loop {
            attempt += 1;
            info!(attempt, "Starting pipeline run");

            // ── Stage 1: Planning ────────────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Planner });
            let plan = planner.execute(prompt).await.map_err(|e| {
                AgentError::ExecutionFailed { role: AgentRole::Planner, message: e.to_string() }
            })?;
            if !plan.success {
                warn!(role = %plan.role, "Planner reported failure; retrying");
                send!(PipelineEvent::PipelineFailed { error: format!("Planner failed: {}", plan.summary) });
                if attempt >= self.max_retries {
                    return Err(AgentError::MaxRetriesExceeded(self.max_retries));
                }
                continue;
            }
            send!(PipelineEvent::StageCompleted { output: plan.clone() });

            // ── Stage 2: Execution (Conductor) ───────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Conductor });
            let context_for_conductor = serde_json::to_string(&plan.payload)?;
            let code_output = conductor.execute(&context_for_conductor).await.map_err(|e| {
                AgentError::ExecutionFailed { role: AgentRole::Conductor, message: e.to_string() }
            })?;
            if !code_output.success {
                warn!(role = %code_output.role, "Conductor reported failure; retrying");
                send!(PipelineEvent::PipelineFailed { error: format!("Conductor failed: {}", code_output.summary) });
                if attempt >= self.max_retries {
                    return Err(AgentError::MaxRetriesExceeded(self.max_retries));
                }
                continue;
            }
            send!(PipelineEvent::StageCompleted { output: code_output.clone() });

            // ── Stage 3: Risk Assessment ─────────────────────────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Risk });
            let risk_context = serde_json::to_string(&code_output.payload)?;
            let risk_output = risk_agent.execute(&risk_context).await.map_err(|e| {
                AgentError::ExecutionFailed { role: AgentRole::Risk, message: e.to_string() }
            })?;
            // Risk is always informational — it never causes a pipeline retry.
            send!(PipelineEvent::StageCompleted { output: risk_output.clone() });

            // ── Stage 4: Review (informed by risk assessment) ────────────────
            send!(PipelineEvent::StageStarted { role: AgentRole::Reviewer });
            // Pass both code changes and risk assessment so the Reviewer has full context.
            let combined_review_context = serde_json::json!({
                "code_changes": code_output.payload,
                "risk_assessment": risk_output.payload,
            });
            let context_for_reviewer = serde_json::to_string(&combined_review_context)?;
            let review = reviewer.execute(&context_for_reviewer).await.map_err(|e| {
                AgentError::ExecutionFailed { role: AgentRole::Reviewer, message: e.to_string() }
            })?;
            if !review.success {
                warn!(role = %review.role, "Reviewer rejected; retrying pipeline");
                send!(PipelineEvent::PipelineFailed { error: format!("Reviewer rejected: {}", review.summary) });
                if attempt >= self.max_retries {
                    return Err(AgentError::MaxRetriesExceeded(self.max_retries));
                }
                continue;
            }
            send!(PipelineEvent::StageCompleted { output: review.clone() });

            // ── All stages passed ────────────────────────────────────────────
            info!(attempt, "Pipeline converged successfully");
            return Ok(vec![plan, code_output, risk_output, review]);
        }
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChunkStream, LlmError, LlmMessage, LlmProvider, LlmResponse, StreamChunk};
    use futures::stream;
    use std::sync::Arc;

    /// A deterministic mock LLM provider for unit tests.
    /// Returns canned JSON responses appropriate for each agent's system prompt.
    struct MockLlmProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockLlmProvider {
        fn provider_name(&self) -> &str {
            "mock"
        }
        fn model_name(&self) -> &str {
            "mock-model"
        }

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
                prompt_tokens: None,
                completion_tokens: None,
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
            _tools: &[crate::llm::ToolDef],
        ) -> Result<crate::llm::LlmCompletion, LlmError> {
            let resp = self.complete(messages).await?;
            Ok(crate::llm::LlmCompletion::Done { text: resp.content, prompt_tokens: None, completion_tokens: None })
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
        assert_eq!(LlmPlannerAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Planner);
        assert_eq!(LlmConductorAgent { llm: Arc::clone(&llm), registry: Arc::new(crate::tools::ToolRegistry::empty()) }.role(), AgentRole::Conductor);
        assert_eq!(LlmRiskAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Risk);
        assert_eq!(LlmReviewerAgent { llm: Arc::clone(&llm) }.role(), AgentRole::Reviewer);
    }
}

