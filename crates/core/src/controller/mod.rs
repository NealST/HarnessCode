//! # Controller Layer
//!
//! The Controller is the "safety harness" above the individual agents.
//! It orchestrates the Judge → Scoper → Planner → Coder → Risk → Reviewer pipeline, enforces
//! retry limits, constructs fresh [`ExecutionGuard`] instances per attempt, and
//! streams real-time [`PipelineEvent`]s to callers.
//!
//! ## Public API
//!
//! | Item | Purpose |
//! |------|---------|
//! | [`Controller`] | Main entry point — call [`Controller::run`] or [`Controller::run_with_progress`] |
//! | [`PipelineEvent`] | Events emitted during pipeline execution |
//! | [`guardrails::ExecutionGuard`] | Safety limits injected into the Coder's tool loop |
//! | [`guardrails::GuardrailViolation`] | Error type for exceeded limits |

#[allow(clippy::module_inception)]
pub mod controller;
pub mod events;
pub mod guardrails;
pub mod interaction;
pub mod request_context;
pub mod tool_loop;

pub use controller::Controller;
pub use events::PipelineEvent;
pub use guardrails::{ExecutionGuard, GuardrailViolation, StepStatus};
pub use interaction::{ClarificationCallback, ClarificationRequest, ClarificationResolution,
					ScoperSkipCallback, ScoperSkipDecision};
pub use request_context::{
	ConversationMessage, ConversationRole, RequestContext, SessionState,
};
