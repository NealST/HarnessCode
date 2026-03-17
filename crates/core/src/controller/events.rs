//! Pipeline events emitted by the Controller to external consumers (e.g. CLI spinner).

use crate::agents::{AgentOutput, AgentRole};
use serde::{Deserialize, Serialize};

/// Events emitted by [`super::Controller::run_with_progress`] as the pipeline runs.
///
/// Consumers receive these over a [`tokio::sync::mpsc`] channel and can drive
/// real-time UI (spinners, progress bars, log lines).
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// An agent stage has started — show a "thinking" indicator.
    StageStarted { role: AgentRole },
    /// An agent stage completed successfully.
    StageCompleted { output: AgentOutput },
    /// The pipeline failed (agent error, guardrail, or max retries exceeded).
    PipelineFailed { error: String },
}

/// Categorises why a pipeline run terminated before completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminationReason {
    MaxRetries,
    Timeout,
    GuardrailViolation(String),
}
