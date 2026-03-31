//! Drift detection types and the `DriftJudgeAgent` — an independent sub-agent
//! that assesses whether the main coder has drifted from the original goal.
//!
//! All drift-related types live here so that both the `agents::coder` and
//! `controller` modules can import them without creating circular dependencies.

pub mod context;

use super::AgentError;
use crate::llm::{LlmMessage, LlmProvider};
use crate::observability::TokenUsage;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ──────────────────────────────────────────────
// Configuration
// ──────────────────────────────────────────────

/// Controls when and how the drift judge is triggered inside the tool loop.
#[derive(Debug, Clone)]
pub struct DriftConfig {
    /// Number of tool-use turns between drift checks (must be ≥ 1, default: 5).
    pub check_interval: usize,
    /// Number of most-recent turns sent to the judge as context (must be ≥ 1, default: 5).
    pub window_size: usize,
    /// Maximum number of drift-triggered restarts before the pipeline gives up
    /// (independent of the normal phase-failure retry budget, default: 3).
    pub max_drift_restarts: usize,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self { check_interval: 5, window_size: 5, max_drift_restarts: 3 }
    }
}

impl DriftConfig {
    /// Create a validated `DriftConfig`.
    ///
    /// Returns `Err` when `check_interval` or `window_size` is 0 (which would
    /// cause a division-by-zero panic or a useless LLM call).
    pub fn new(
        check_interval: usize,
        window_size: usize,
        max_drift_restarts: usize,
    ) -> Result<Self, &'static str> {
        if check_interval == 0 {
            return Err("DriftConfig: check_interval must be ≥ 1");
        }
        if window_size == 0 {
            return Err("DriftConfig: window_size must be ≥ 1");
        }
        Ok(Self { check_interval, window_size, max_drift_restarts })
    }
}

// ──────────────────────────────────────────────
// Signal and decision types
// ──────────────────────────────────────────────

/// Classification of the kind of drift the judge observed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriftKind {
    /// Agent is doing work unrelated to or beyond the original goal.
    Scope,
    /// Agent is moving in a direction that no longer serves the original goal
    /// (e.g. stuck in a loop, refactoring instead of fixing).
    Direction,
    /// Both scope and direction drift are present.
    Both,
}

impl std::fmt::Display for DriftKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriftKind::Scope     => write!(f, "scope"),
            DriftKind::Direction => write!(f, "direction"),
            DriftKind::Both      => write!(f, "both"),
        }
    }
}

/// The result of a single drift-judge call.
#[derive(Debug, Clone)]
pub enum DriftSignal {
    /// Agent activity is aligned with the original goal.
    Aligned,
    /// Agent has drifted from the original goal.
    Drifted { kind: DriftKind, reason: String },
}

/// The user's response after being presented with a drift warning.
#[derive(Debug, Clone)]
pub enum DriftDecision {
    /// Abort the pipeline run entirely.
    Stop,
    /// Restart the pipeline with a prompt reinforced with anti-drift context.
    Restart,
    /// Dismiss the warning and let the agent continue.
    Ignore,
}

// ──────────────────────────────────────────────
// Turn summary (input to the judge)
// ──────────────────────────────────────────────

/// Lightweight summary of one tool-use turn, forwarded to the drift judge.
#[derive(Debug, Clone, Serialize)]
pub struct TurnSummary {
    pub turn_number: usize,
    pub tools_called: Vec<String>,
    /// Truncated JSON representation of tool call arguments (≤ 500 chars).
    pub call_args_snippet: String,
}

// ──────────────────────────────────────────────
// Async callback type
// ──────────────────────────────────────────────

/// Async callback invoked when drift is detected inside the tool loop.
///
/// Receives the [`DriftSignal`] and returns the user's [`DriftDecision`].
/// Kept as a trait-object callback so the `harnesscode-core` crate stays
/// free of any Tauri or UI dependencies.
pub type DriftCallback = Arc<
    dyn Fn(DriftSignal) -> Pin<Box<dyn Future<Output = DriftDecision> + Send>>
        + Send
        + Sync,
>;

/// Synchronous notification hook called immediately *before* invoking the
/// async `DriftCallback`.  Receives the drift `kind` and `reason` strings.
///
/// Intended for emitting a `PipelineEvent::DriftDetected` (or similar) to
/// a UI channel.  The controller builds this closure capturing the mpsc
/// sender so that `agents::drift_judge` remains free of controller deps.
pub type DriftNotifyFn = Arc<dyn Fn(&DriftKind, &str) + Send + Sync>;

// ──────────────────────────────────────────────
// Bundled params (passed into run_tool_loop)
// ──────────────────────────────────────────────

/// All drift-detection parameters bundled together for passing into
/// [`crate::controller::tool_loop::run_tool_loop`].
pub struct DriftParams {
    pub judge: Arc<DriftJudgeAgent>,
    pub config: DriftConfig,
    /// Clean original user prompt — embedded verbatim in drift-reinforced restart
    /// messages so the re-attempt always targets the unmodified user intent.
    pub original_prompt: String,
    /// Phase-scoped prompt sent to the judge LLM.  Extends `original_prompt`
    /// with the current phase objective and scope boundaries so the judge can
    /// tell legitimate phase work from true drift.
    pub judge_prompt: String,
    pub callback: DriftCallback,
    /// Optional synchronous hook fired *before* `callback`; use it to emit
    /// a `PipelineEvent::DriftDetected` without creating a circular dep.
    pub notify: Option<DriftNotifyFn>,
}

// ──────────────────────────────────────────────
// Judge agent
// ──────────────────────────────────────────────

const SYSTEM_PROMPT: &str = context::SYSTEM;

/// An independent drift-detection sub-agent.
///
/// Unlike the main pipeline agents, `DriftJudgeAgent`:
/// * Has its own `llm` field — can be configured to a cheaper/faster model.
/// * Does **not** implement the generic [`Agent`] trait (different I/O shape).
/// * Makes a single structured LLM call (no tool loop).
pub struct DriftJudgeAgent {
    pub llm: Arc<dyn LlmProvider>,
}

impl DriftJudgeAgent {
    /// Assess whether recent tool activity is still aligned with `original_prompt`.
    /// Returns the drift signal together with the LLM token usage for the call
    /// so callers can include it in observability spans.
    pub async fn judge(
        &self,
        original_prompt: &str,
        turns: &[TurnSummary],
    ) -> Result<(DriftSignal, TokenUsage), AgentError> {
        let user_content = serde_json::json!({
            "original_goal": original_prompt,
            "recent_turns": turns,
        })
        .to_string();

        let messages = vec![
            LlmMessage::system(SYSTEM_PROMPT),
            LlmMessage::user(user_content),
        ];

        let response = self.llm.complete(&messages).await?;
        let usage = TokenUsage::from(&response);
        let signal = parse_drift_signal(&response.content)?;
        Ok((signal, usage))
    }
}

// ──────────────────────────────────────────────
// Private parsing helpers
// ──────────────────────────────────────────────

fn parse_drift_signal(text: &str) -> Result<DriftSignal, AgentError> {
    let json_str = extract_json(text);
    let v: serde_json::Value = serde_json::from_str(json_str)?;

    // BUG-4 fix: absent `aligned` key must default to `false`, not `true`.
    // Defaulting to `true` would silently treat malformed responses as Aligned.
    if v["aligned"].as_bool().unwrap_or(false) {
        return Ok(DriftSignal::Aligned);
    }

    // BUG-5 fix: when `kind` is absent default to `Both` (most conservative)
    // rather than `Scope`, to avoid misclassifying an unknown signal.
    let kind = match v["kind"].as_str().unwrap_or("both") {
        "scope"     => DriftKind::Scope,
        "direction" => DriftKind::Direction,
        _           => DriftKind::Both,
    };

    let reason = v["reason"]
        .as_str()
        .unwrap_or("Drift detected")
        .to_string();

    Ok(DriftSignal::Drifted { kind, reason })
}

/// Extract the first complete `{…}` JSON object from `text` using a
/// bracket-depth counter.
///
/// Unlike `rfind('}')`, this correctly handles `}` characters that appear
/// *inside* string values (e.g. in the `reason` field).
fn extract_json(text: &str) -> &str {
    let t = text.trim();
    let Some(start) = t.find('{') else { return t };
    let bytes = t.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape_next = true,
            b'"'               => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return &t[start..=start + i];
                }
            }
            _ => {}
        }
    }
    // No balanced closing brace — return everything from the opening brace.
    &t[start..]
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChunkStream, LlmCompletion, LlmError, LlmResponse, ToolDef};

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
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
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

    fn one_turn() -> Vec<TurnSummary> {
        vec![TurnSummary {
            turn_number: 1,
            tools_called: vec!["read_file".into()],
            call_args_snippet: r#"{"path":"main.rs"}"#.into(),
        }]
    }

    #[tokio::test]
    async fn judge_drift_direction() {
        let llm = Arc::new(MockLlm {
            response: r#"{"aligned":false,"kind":"direction","reason":"Agent is looping on read_file"}"#.into(),
        });
        let judge = DriftJudgeAgent { llm };
        let (signal, _tokens) = judge.judge("Fix the bug in main.rs", &one_turn()).await.unwrap();
        match signal {
            DriftSignal::Drifted { kind: DriftKind::Direction, reason } => {
                assert!(reason.contains("loop"));
            }
            other => panic!("expected Drifted(Direction), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn judge_aligned() {
        let llm = Arc::new(MockLlm {
            response: r#"{"aligned":true}"#.into(),
        });
        let judge = DriftJudgeAgent { llm };
        let (signal, _tokens) = judge.judge("Fix the bug in main.rs", &[]).await.unwrap();
        assert!(matches!(signal, DriftSignal::Aligned));
    }

    #[tokio::test]
    async fn judge_with_markdown_fences() {
        let llm = Arc::new(MockLlm {
            response: "```json\n{\"aligned\":false,\"kind\":\"scope\",\"reason\":\"unrelated work\"}\n```".into(),
        });
        let judge = DriftJudgeAgent { llm };
        let (signal, _tokens) = judge.judge("Fix the bug", &[]).await.unwrap();
        assert!(matches!(
            signal,
            DriftSignal::Drifted { kind: DriftKind::Scope, .. }
        ));
    }
}
