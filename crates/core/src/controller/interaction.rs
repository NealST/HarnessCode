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

/// Whether the user wants the Scoper agent to run or to be skipped.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoperSkipDecision {
    /// Run the Scoper agent to produce a full structured problem frame.
    Run,
    /// Skip the Scoper and use a lightweight synthetic scope so the pipeline
    /// proceeds directly to planning.
    Skip,
}

/// Async callback invoked by the controller just before the Scoper would
/// start, giving the user a chance to bypass it.
///
/// `effective_request` is the Judge-resolved prompt shown to the user so they
/// can make an informed decision.  Return [`ScoperSkipDecision::Skip`] to skip,
/// [`ScoperSkipDecision::Run`] to let the Scoper proceed normally.
pub type ScoperSkipCallback = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = ScoperSkipDecision> + Send>>
        + Send
        + Sync,
>;