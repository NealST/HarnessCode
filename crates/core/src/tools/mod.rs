//! # Tool Layer
//!
//! Provides the [`Tool`] trait, [`ToolResult`], and the [`ToolRegistry`] that
//! agents use to interact with the real world (filesystem, shell, etc.).
//!
//! ## Architecture
//!
//! The tool layer is the **deterministic half** of the Harness Engineering model.
//! While agents reason probabilistically, tools execute with guaranteed fidelity:
//!
//! * Sensors  — [`ReadFileTool`], [`ListDirectoryTool`], [`SearchFilesTool`]
//! * Actuators — [`WriteFileTool`], [`ApplyDiffTool`], [`RunCommandTool`]
//!
//! ## Builtin tools
//!
//! | Tool | Role |
//! |------|------|
//! | [`ReadFileTool`]       | Read a file's contents (sensor) |
//! | [`WriteFileTool`]      | Write / overwrite a file (actuator) |
//! | [`ApplyDiffTool`]      | Apply a unified diff (actuator) |
//! | [`ListDirectoryTool`]  | List directory contents (sensor) |
//! | [`SearchFilesTool`]    | Full-text regex search across the workspace (sensor) |
//! | [`RunCommandTool`]     | Execute a whitelisted shell command (actuator/sensor) |

pub mod read_file;
pub mod write_file;
pub mod apply_diff;
pub mod list_directory;
pub mod search_files;
pub mod run_command;

pub use read_file::ReadFileTool;
pub use write_file::WriteFileTool;
pub use apply_diff::ApplyDiffTool;
pub use list_directory::ListDirectoryTool;
pub use search_files::SearchFilesTool;
pub use run_command::RunCommandTool;

use crate::llm::ToolDef;
use async_trait::async_trait;
use std::collections::HashMap;

// ──────────────────────────────────────────────
// Core types
// ──────────────────────────────────────────────

/// The result returned by a tool after execution.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Content to feed back to the LLM (stdout, file contents, error message…).
    pub content: String,
    /// `true` when the tool failed — the LLM can decide to retry or give up.
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self { content: message.into(), is_error: true }
    }
}

// ──────────────────────────────────────────────
// Tool trait
// ──────────────────────────────────────────────

/// Every builtin (and future user-defined) tool must implement this trait.
///
/// Tools are **stateless** — all context is passed through `args` and the
/// working directory is inherited from the process.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The static definition (name, description, JSON Schema) sent to the LLM.
    fn def(&self) -> ToolDef;

    /// Execute the tool with the given arguments.
    ///
    /// Implementations should never panic.  All errors must be returned as
    /// `ToolResult::err(...)`.
    async fn call(&self, args: serde_json::Value) -> ToolResult;
}

// ──────────────────────────────────────────────
// ToolRegistry
// ──────────────────────────────────────────────

/// A runtime registry of all tools available to agents.
///
/// Constructed once per [`Controller`](crate::multi_agent::Controller) run.
/// Provides `defs()` for the LLM to select from, and `dispatch()` to execute.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry (useful for testing or custom toolsets).
    pub fn empty() -> Self {
        Self { tools: HashMap::new() }
    }

    /// Create a registry pre-loaded with all six builtin tools.
    pub fn with_builtins() -> Self {
        let mut r = Self::empty();
        r.register(ReadFileTool);
        r.register(WriteFileTool);
        r.register(ApplyDiffTool);
        r.register(ListDirectoryTool);
        r.register(SearchFilesTool);
        r.register(RunCommandTool);
        r
    }

    /// Create a registry with read-only sensor tools only (no actuators).
    ///
    /// Used by the Planner agent to safely explore the codebase before writing
    /// a grounded execution plan.  Write and shell-execution tools are excluded.
    pub fn with_sensors() -> Self {
        let mut r = Self::empty();
        r.register(ReadFileTool);
        r.register(ListDirectoryTool);
        r.register(SearchFilesTool);
        r
    }

    /// Register an additional tool (builtin or user-defined skill).
    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.def().name.clone(), Box::new(tool));
    }

    /// The ordered list of tool definitions to include in an LLM request.
    pub fn defs(&self) -> Vec<ToolDef> {
        let mut defs: Vec<_> = self.tools.values().map(|t| t.def()).collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name)); // stable ordering
        defs
    }

    /// Dispatch a tool call by name.  Returns an error result if the tool is unknown.
    pub async fn dispatch(&self, call: &crate::llm::ToolCall) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(tool) => tool.call(call.arguments.clone()).await,
            None => ToolResult::err(format!("Unknown tool: '{}'", call.name)),
        }
    }
}
