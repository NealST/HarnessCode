//! Drift detection types and the `DriftJudgeAgent` — an independent sub-agent
//! that assesses whether the main coder has drifted from the original goal.
//!
//! All drift-related types live here so that both the `agents::coder` and
//! `controller` modules can import them without creating circular dependencies.

pub mod context;

use super::AgentError;
use crate::llm::{LlmMessage, LlmProvider};
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
    /// Number of tool-use turns between drift checks (default: 5, user-configurable).
    pub check_interval: usize,
    /// Number of most-recent turns sent to the judge as context (default: 5).
    pub window_size: usize,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self { check_interval: 5, window_size: 5 }
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

// ──────────────────────────────────────────────
// Bundled params (passed into run_tool_loop)
// ──────────────────────────────────────────────

/// All drift-detection parameters bundled together for passing into
/// [`crate::controller::tool_loop::run_tool_loop`].
pub struct DriftParams {
    pub judge: Arc<DriftJudgeAgent>,
    pub config: DriftConfig,
    /// Copy of the original user prompt embedded in reinforcement messages.
    pub original_prompt: String,
    pub callback: DriftCallback,
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
    pub async fn judge(
        &self,
        original_prompt: &str,
        turns: &[TurnSummary],
    ) -> Result<DriftSignal, AgentError> {
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
        parse_drift_signal(&response.content)
    }
}

// ──────────────────────────────────────────────
// Private parsing helpers
// ──────────────────────────────────────────────

fn parse_drift_signal(text: &str) -> Result<DriftSignal, AgentError> {
    let json_str = extract_json(text);
    let v: serde_json::Value = serde_json::from_str(json_str)?;

    if v["aligned"].as_bool().unwrap_or(true) {
        return Ok(DriftSignal::Aligned);
    }

    let kind = match v["kind"].as_str().unwrap_or("scope") {
        "direction" => DriftKind::Direction,
        "both"      => DriftKind::Both,
        _           => DriftKind::Scope,
    };

    let reason = v["reason"]
        .as_str()
        .unwrap_or("Drift detected")
        .to_string();

    Ok(DriftSignal::Drifted { kind, reason })
}

/// Strip optional markdown fences and return a slice pointing at the first
/// `{…}` JSON object in the text.
fn extract_json(text: &str) -> &str {
    let t = text.trim();
    match (t.find('{'), t.rfind('}')) {
        (Some(start), Some(end)) if end >= start => &t[start..=end],
        _ => t,
    }
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
        let signal = judge.judge("Fix the bug in main.rs", &one_turn()).await.unwrap();
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
        let signal = judge.judge("Fix the bug in main.rs", &[]).await.unwrap();
        assert!(matches!(signal, DriftSignal::Aligned));
    }

    #[tokio::test]
    async fn judge_with_markdown_fences() {
        let llm = Arc::new(MockLlm {
            response: "```json\n{\"aligned\":false,\"kind\":\"scope\",\"reason\":\"unrelated work\"}\n```".into(),
        });
        let judge = DriftJudgeAgent { llm };
        let signal = judge.judge("Fix the bug", &[]).await.unwrap();
        assert!(matches!(
            signal,
            DriftSignal::Drifted { kind: DriftKind::Scope, .. }
        ));
    }
}
