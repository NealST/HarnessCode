//! # LLM Provider Abstraction
//!
//! Pluggable LLM backend layer. All agent code calls the [`LlmProvider`] trait,
//! never a concrete HTTP client directly.
//!
//! ## Supported providers
//!
//! | Module        | Provider                              |
//! |---------------|---------------------------------------|
//! | [`openai`]    | OpenAI (GPT-4o, GPT-4-turbo, etc.)   |
//! | [`anthropic`] | Anthropic (Claude 3.5 Sonnet, etc.)  |

pub mod anthropic;
pub mod openai_compatible;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;

// ──────────────────────────────────────────────
// Message types
// ──────────────────────────────────────────────

/// Role of a message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

/// A single message in an LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: MessageRole,
    pub content: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: MessageRole::System, content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: MessageRole::User, content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: MessageRole::Assistant, content: content.into() }
    }
}

// ──────────────────────────────────────────────
// Response types
// ──────────────────────────────────────────────

/// A complete (non-streaming) response from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    pub model: String,
    /// Total tokens consumed (if the provider reports usage).
    pub total_tokens: Option<u32>,
}

/// A single chunk emitted during a streaming LLM response.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Incremental text delta.
    pub delta: String,
    /// `true` on the final chunk — the stream is complete.
    pub finished: bool,
}

// ──────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("Missing API key for provider '{provider}'. Set the {env_var} environment variable.")]
    MissingApiKey {
        provider: &'static str,
        env_var: &'static str,
    },

    #[error("Failed to parse LLM response: {0}")]
    Parse(String),

    #[error("Streaming error: {0}")]
    Stream(String),
}

// ──────────────────────────────────────────────
// Provider trait
// ──────────────────────────────────────────────

/// A pinned, boxed async stream of [`StreamChunk`] results.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, LlmError>> + Send>>;

/// Every LLM backend must implement this trait.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// The provider identifier (e.g. `"openai"`, `"anthropic"`).
    fn provider_name(&self) -> &str;

    /// The model name in use (e.g. `"gpt-4o"`, `"claude-3-5-sonnet-20241022"`).
    fn model_name(&self) -> &str;

    /// Send a list of messages and await the full response.
    async fn complete(&self, messages: &[LlmMessage]) -> Result<LlmResponse, LlmError>;

    /// Send a list of messages and receive the response as a streaming sequence of chunks.
    async fn stream(&self, messages: &[LlmMessage]) -> Result<ChunkStream, LlmError>;
}
