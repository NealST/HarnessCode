use crate::agents::AgentRole;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A clarification prompt that must be answered before the pipeline can continue.
#[derive(Debug, Clone)]
pub struct ClarificationRequest {
    pub source: AgentRole,
    pub questions: Vec<String>,
    pub objective: String,
}

/// User response to a clarification prompt.
#[derive(Debug, Clone)]
pub enum ClarificationResolution {
    Abort,
    Answer(String),
}

/// Async callback invoked when the pipeline needs additional user input.
pub type ClarificationCallback = Arc<
    dyn Fn(ClarificationRequest) -> Pin<Box<dyn Future<Output = ClarificationResolution> + Send>>
        + Send
        + Sync,
>;