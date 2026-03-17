//! Anthropic Messages API provider (Claude 3.5 Sonnet, etc.)
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

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

// ── Request shapes ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
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
struct AnthropicResponse {
    model: String,
    content: Vec<ContentBlock>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ── Streaming event shapes ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    delta: Option<StreamDelta>,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(rename = "type")]
    delta_type: String,
    text: Option<String>,
}

// ── Provider ──────────────────────────────────────────────────────────────────

/// Anthropic Messages API provider.
///
/// Configuration is always injected via [`Self::with_config`]; this struct
/// never reads environment variables itself.
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    /// Create with explicit credentials.
    pub fn with_config(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn build_request<'a>(
        &'a self,
        messages: &'a [LlmMessage],
        stream: Option<bool>,
    ) -> AnthropicRequest<'a> {
        // Anthropic separates the system message from the conversation turns.
        let system = messages
            .iter()
            .find(|m| m.role == MessageRole::System)
            .map(|m| m.content.as_str());

        let api_messages = messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .map(|m| ApiMessage {
                role: match m.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    // System messages are filtered above; Tool-role messages
                    // are handled via complete_with_tools, not this path.
                    MessageRole::System | MessageRole::Tool => unreachable!(),
                },
                content: &m.content,
            })
            .collect();

        AnthropicRequest {
            model: &self.model,
            max_tokens: DEFAULT_MAX_TOKENS,
            system,
            messages: api_messages,
            stream,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, messages: &[LlmMessage]) -> Result<LlmResponse, LlmError> {
        info!(model = %self.model, "Anthropic: sending completion request");
        let body = self.build_request(messages, None);

        let resp = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: msg });
        }

        let response: AnthropicResponse = resp.json().await?;
        let content = response
            .content
            .into_iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");

        let (prompt_tokens, completion_tokens, total_tokens) = response.usage
            .map(|u| {
                let total = u.input_tokens + u.output_tokens;
                (Some(u.input_tokens), Some(u.output_tokens), Some(total))
            })
            .unwrap_or((None, None, None));

        debug!(tokens = ?total_tokens, "Anthropic completion done");

        Ok(LlmResponse {
            content,
            model: response.model,
            prompt_tokens,
            completion_tokens,
            total_tokens,
        })
    }

    async fn stream(&self, messages: &[LlmMessage]) -> Result<ChunkStream, LlmError> {
        info!(model = %self.model, "Anthropic: opening streaming request");
        let body = self.build_request(messages, Some(true));

        let resp = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await?;

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
                            match serde_json::from_str::<StreamEvent>(data) {
                                Ok(event) => {
                                    let finished = event.event_type == "message_stop";
                                    let delta = event
                                        .delta
                                        .filter(|d| d.delta_type == "text_delta")
                                        .and_then(|d| d.text)
                                        .unwrap_or_default();
                                    // Skip keep-alive events that carry no text
                                    if delta.is_empty() && !finished {
                                        None
                                    } else {
                                        Some(Ok(StreamChunk { delta, finished }))
                                    }
                                }
                                // Non-parseable lines (e.g. ping events) are silently skipped.
                                Err(_) => None,
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
        info!(model = %self.model, tools = tools.len(), "Anthropic: complete_with_tools");

        // Extract system message (Anthropic keeps it separate).
        let system = messages
            .iter()
            .find(|m| m.role == MessageRole::System)
            .map(|m| m.content.as_str());

        // Serialise conversation turns — tool use messages have special shapes.
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .map(|m| match m.role {
                // Tool result: Anthropic uses a content-block list inside a "user" turn.
                MessageRole::Tool => serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                        "content": m.content,
                        "is_error": m.is_error.unwrap_or(false),
                    }]
                }),
                // Assistant turn that contains tool_use blocks.
                MessageRole::Assistant if m.tool_calls.is_some() => {
                    let blocks: Vec<serde_json::Value> = m.tool_calls.as_ref().unwrap().iter().map(|tc| {
                        serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        })
                    }).collect();
                    serde_json::json!({ "role": "assistant", "content": blocks })
                }
                MessageRole::User      => serde_json::json!({ "role": "user",      "content": m.content }),
                MessageRole::Assistant => serde_json::json!({ "role": "assistant", "content": m.content }),
                MessageRole::System    => unreachable!(),
            })
            .collect();

        // Serialise tool definitions — Anthropic uses `input_schema` instead of `parameters`.
        let api_tools: Vec<serde_json::Value> = tools.iter().map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        }).collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "messages": api_messages,
            "tools": api_tools,
        });
        if let Some(sys) = system {
            body["system"] = serde_json::Value::String(sys.to_string());
        }

        let resp = self.client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: msg });
        }

        // Parse the response — could be text blocks or tool_use blocks.
        let raw: serde_json::Value = resp.json().await?;
        let stop_reason = raw["stop_reason"].as_str().unwrap_or("end_turn");
        let content = raw["content"].as_array().cloned().unwrap_or_default();

        let prompt_tokens = raw["usage"]["input_tokens"].as_u64().map(|v| v as u32);
        let completion_tokens = raw["usage"]["output_tokens"].as_u64().map(|v| v as u32);

        if stop_reason == "tool_use" {
            let calls: Vec<ToolCall> = content
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| ToolCall {
                    id: b["id"].as_str().unwrap_or("").to_string(),
                    name: b["name"].as_str().unwrap_or("").to_string(),
                    arguments: b["input"].clone(),
                })
                .collect();
            Ok(LlmCompletion::NeedTools { calls, prompt_tokens, completion_tokens })
        } else {
            let text: String = content
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            Ok(LlmCompletion::Done { text, prompt_tokens, completion_tokens })
        }
    }
}
