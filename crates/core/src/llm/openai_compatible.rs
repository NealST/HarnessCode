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

use super::{
    ChunkStream, LlmCompletion, LlmError, LlmMessage, LlmProvider, LlmResponse,
    MessageRole, StreamChunk, ToolCall, ToolDef,
};
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
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
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

// ── Tool-calling request / response shapes ────────────────────────────────────

#[derive(Serialize)]
#[allow(dead_code)]
struct ChatRequestWithTools<'a> {
    model: &'a str,
    messages: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct ToolChoice {
    choices: Vec<ToolChoiceItem>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ToolChoiceItem {
    message: ToolChoiceMessage,
    finish_reason: String,
}

#[derive(Deserialize, Default)]
struct ToolChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Deserialize)]
struct ApiToolCall {
    id: String,
    function: ApiToolCallFunction,
}

#[derive(Deserialize)]
struct ApiToolCallFunction {
    name: String,
    arguments: String,
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
                    // Tool-result messages use a different shape; the simple
                    // to_api_messages helper is only used by complete() and
                    // stream() which never include Tool-role messages.
                    MessageRole::Tool => "tool",
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
        let (prompt_tokens, completion_tokens, total_tokens) = body.usage
            .map(|u| (u.prompt_tokens, u.completion_tokens, u.total_tokens))
            .unwrap_or((None, None, None));
        let content = body
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| LlmError::Parse("no choices returned".into()))?;

        debug!(tokens = ?total_tokens, "OpenAI-compat completion done");

        Ok(LlmResponse {
            content,
            model: body.model,
            prompt_tokens,
            completion_tokens,
            total_tokens,
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

    async fn complete_with_tools(
        &self,
        messages: &[LlmMessage],
        tools: &[ToolDef],
    ) -> Result<LlmCompletion, LlmError> {
        info!(model = %self.model, tools = tools.len(), "OpenAI-compat: complete_with_tools");
        let url = format!("{}/chat/completions", self.base_url);

        // Serialise messages — tool roles need special shapes.
        let api_messages: Vec<serde_json::Value> = messages.iter().map(|m| {
            match m.role {
                MessageRole::Tool => serde_json::json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.as_deref().unwrap_or(""),
                    "content": m.content,
                }),
                MessageRole::Assistant if m.tool_calls.is_some() => {
                    let calls: Vec<serde_json::Value> = m.tool_calls.as_ref().unwrap().iter().map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments.to_string(),
                            }
                        })
                    }).collect();
                    serde_json::json!({ "role": "assistant", "content": null, "tool_calls": calls })
                }
                MessageRole::System    => serde_json::json!({ "role": "system",    "content": m.content }),
                MessageRole::User      => serde_json::json!({ "role": "user",      "content": m.content }),
                MessageRole::Assistant => serde_json::json!({ "role": "assistant", "content": m.content }),
            }
        }).collect();

        // Serialise tool definitions to OpenAI format.
        let api_tools: Vec<serde_json::Value> = tools.iter().map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        }).collect();

        let body = serde_json::json!({
            "model": self.model,
            "messages": api_messages,
            "tools": api_tools,
        });

        let mut req = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: msg });
        }

        let body: ToolChoice = resp.json().await?;
        let prompt_tokens = body.usage.as_ref().and_then(|u| u.prompt_tokens);
        let completion_tokens = body.usage.as_ref().and_then(|u| u.completion_tokens);
        let choice = body.choices.into_iter().next()
            .ok_or_else(|| LlmError::Parse("no choices in tool response".into()))?;

        if choice.finish_reason == "tool_calls" {
            let calls = choice.message.tool_calls.unwrap_or_default()
                .into_iter()
                .map(|tc| {
                    let arguments = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    ToolCall { id: tc.id, name: tc.function.name, arguments }
                })
                .collect();
            Ok(LlmCompletion::NeedTools { calls, prompt_tokens, completion_tokens })
        } else {
            let text = choice.message.content.unwrap_or_default();
            Ok(LlmCompletion::Done { text, prompt_tokens, completion_tokens })
        }
    }
}
