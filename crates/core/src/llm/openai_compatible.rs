//! Generic OpenAI-compatible Chat Completions provider.
//!
//! Works with any API that speaks the OpenAI Chat Completions protocol:
//! - OpenAI (`https://api.openai.com/v1`)
//! - Azure OpenAI
//! - Ollama (`http://localhost:11434/v1`)
//! - DeepSeek (`https://api.deepseek.com/v1`)
//! - Groq, Together, Anyscale, etc.
//!
//! Configuration is injected externally (from [`crate::config`]) — this provider
//! never reads environment variables directly.

use super::{ChunkStream, LlmError, LlmMessage, LlmProvider, LlmResponse, MessageRole, StreamChunk};
use async_trait::async_trait;
use futures::{stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ── Request shapes ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ApiMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'static str,
    content: &'a str,
}

// ── Non-streaming response shapes ────────────────────────────────────────────

#[derive(Deserialize)]
struct ChatResponse {
    model: String,
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct Usage {
    total_tokens: u32,
}

// ── Streaming response shapes ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamResponse {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
}

// ── Provider ──────────────────────────────────────────────────────────────────

/// OpenAI-compatible Chat Completions provider.
///
/// Configuration is always injected via [`Self::with_config`]; this struct
/// never reads environment variables itself.
pub struct OpenAICompatibleProvider {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAICompatibleProvider {
    /// Create a provider with explicit credentials.
    ///
    /// `api_key` may be empty for providers that don't require authentication
    /// (e.g. a local Ollama instance).
    pub fn with_config(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: base_url.into(),
        }
    }

    fn to_api_messages<'a>(&self, messages: &'a [LlmMessage]) -> Vec<ApiMessage<'a>> {
        messages
            .iter()
            .map(|m| ApiMessage {
                role: match m.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect()
    }
}

#[async_trait]
impl LlmProvider for OpenAICompatibleProvider {
    fn provider_name(&self) -> &str {
        "openai-compatible"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, messages: &[LlmMessage]) -> Result<LlmResponse, LlmError> {
        info!(model = %self.model, base_url = %self.base_url, "OpenAI-compat: sending completion request");
        let url = format!("{}/chat/completions", self.base_url);

        let mut req = self
            .client
            .post(&url)
            .json(&ChatRequest {
                model: &self.model,
                messages: self.to_api_messages(messages),
                stream: None,
            });

        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: msg });
        }

        let body: ChatResponse = resp.json().await?;
        let total = body.usage.map(|u| u.total_tokens);
        let content = body
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| LlmError::Parse("no choices returned".into()))?;

        debug!(tokens = ?total, "OpenAI-compat completion done");

        Ok(LlmResponse {
            content,
            model: body.model,
            total_tokens: total,
        })
    }

    async fn stream(&self, messages: &[LlmMessage]) -> Result<ChunkStream, LlmError> {
        info!(model = %self.model, base_url = %self.base_url, "OpenAI-compat: opening streaming request");
        let url = format!("{}/chat/completions", self.base_url);

        let mut req = self
            .client
            .post(&url)
            .json(&ChatRequest {
                model: &self.model,
                messages: self.to_api_messages(messages),
                stream: Some(true),
            });

        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: msg });
        }

        let byte_stream = resp.bytes_stream();
        let chunk_stream = byte_stream.flat_map(|result| {
            let items: Vec<Result<StreamChunk, LlmError>> = match result {
                Err(e) => vec![Err(LlmError::Http(e))],
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    text.lines()
                        .filter(|line| line.starts_with("data: "))
                        .filter_map(|line| {
                            let data = &line["data: ".len()..];
                            if data == "[DONE]" {
                                return Some(Ok(StreamChunk {
                                    delta: String::new(),
                                    finished: true,
                                }));
                            }
                            match serde_json::from_str::<StreamResponse>(data) {
                                Ok(sr) => {
                                    let choice = sr.choices.into_iter().next()?;
                                    let delta = choice.delta.content.unwrap_or_default();
                                    let finished = choice
                                        .finish_reason
                                        .as_deref()
                                        .map(|r| !r.is_empty())
                                        .unwrap_or(false);
                                    Some(Ok(StreamChunk { delta, finished }))
                                }
                                Err(e) => Some(Err(LlmError::Parse(e.to_string()))),
                            }
                        })
                        .collect()
                }
            };
            stream::iter(items)
        });

        Ok(Box::pin(chunk_stream))
    }
}
