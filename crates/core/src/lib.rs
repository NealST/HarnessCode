//! # HarnessCode Core
//!
//! This crate is the shared engine powering every HarnessCode interface (CLI, desktop, …).
//! It intentionally contains **no** CLI or GUI code — only pure domain logic.
//!
//! ## Modules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`agents`] | Agent trait, concrete agent implementations (Planner, Coder, Risk, Reviewer) |
//! | [`controller`] | Cybernetic [`Controller`](controller::Controller), request context, guardrails, pipeline events |
//! | [`observability`] | Span types, [`SpanSink`](observability::SpanSink) trait, [`TerminalSink`](observability::TerminalSink), [`JsonLinesSink`](observability::JsonLinesSink) |
//! | [`context`] | Helpers for generating and parsing `agents.md` / `Claude.md` context files |
//! | [`llm`] | LLM provider trait and vendor adapters |
//! | [`memory`] | Multi-session conversation memory storage and persistence |
//! | [`tools`] | Tool registry and builtin tool implementations |

pub mod agents;
pub mod commands;
pub mod config;
pub mod context;
pub mod controller;
pub mod llm;
pub mod memory;
pub mod observability;
pub mod skills;
pub mod tools;
