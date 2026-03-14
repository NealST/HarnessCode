//! # HarnessCode Core
//!
//! This crate is the shared engine powering every HarnessCode interface (CLI, desktop, …).
//! It intentionally contains **no** CLI or GUI code — only pure domain logic.
//!
//! ## Modules
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`multi_agent`] | Agent traits and the cybernetic [`Controller`](multi_agent::Controller) |
//! | [`risk_management`] | [`RiskManager`](risk_management::RiskManager) that scores file changes |
//! | [`context`] | Helpers for generating and parsing `agents.md` / `Claude.md` context files |

pub mod context;
pub mod multi_agent;
pub mod risk_management;
