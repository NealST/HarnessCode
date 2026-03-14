//! # Context Management
//!
//! This module handles the generation and parsing of HarnessCode's two context
//! files that are shared across agents:
//!
//! * **`agents.md`** — A machine-and-human-readable document that describes the
//!   current task, the plan steps, and each agent's most recent output.
//! * **`Claude.md`** (or `AGENTS.md`) — A project-level context file that the
//!   Planner writes and the Coder/Reviewer read to understand project conventions.
//!
//! Both formats are plain Markdown so they can be committed to version control
//! and inspected by humans at any time.

use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────
// Data structures
// ──────────────────────────────────────────────

/// A single step entry recorded in `agents.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    /// 1-based index of the step.
    pub index: usize,
    /// Short description of what this step accomplishes.
    pub description: String,
    /// Whether the step has been completed.
    pub completed: bool,
}

/// The full in-memory representation of an `agents.md` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsContext {
    /// The original user prompt / task description.
    pub task: String,
    /// Ordered list of plan steps produced by the Planner.
    pub steps: Vec<PlanStep>,
    /// Free-form notes appended by agents during execution.
    pub notes: Vec<String>,
}

impl AgentsContext {
    /// Create a new context for the given `task`.
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            steps: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Append a plan step.
    pub fn add_step(&mut self, description: impl Into<String>) {
        let index = self.steps.len() + 1;
        self.steps.push(PlanStep {
            index,
            description: description.into(),
            completed: false,
        });
    }

    /// Mark step at `index` (1-based) as completed.
    pub fn complete_step(&mut self, index: usize) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.index == index) {
            step.completed = true;
        }
    }

    /// Render this context as a Markdown string suitable for writing to `agents.md`.
    pub fn to_markdown(&self) -> String {
        let mut md = format!("# agents.md\n\n## Task\n\n{}\n\n## Plan\n\n", self.task);

        for step in &self.steps {
            let checkbox = if step.completed { "[x]" } else { "[ ]" };
            md.push_str(&format!("- {} {}\n", checkbox, step.description));
        }

        if !self.notes.is_empty() {
            md.push_str("\n## Notes\n\n");
            for note in &self.notes {
                md.push_str(&format!("- {}\n", note));
            }
        }

        md
    }
}

// ──────────────────────────────────────────────
// Claude.md / AGENTS.md generator
// ──────────────────────────────────────────────

/// Project-level context provided to every agent at the start of a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Project name.
    pub name: String,
    /// Brief description of the project.
    pub description: String,
    /// Key conventions the agents must follow (e.g. style, testing).
    pub conventions: Vec<String>,
    /// Files / paths the agents should treat as off-limits.
    pub restricted_paths: Vec<String>,
}

impl ProjectContext {
    /// Render this context as the content of a `Claude.md` file.
    pub fn to_claude_md(&self) -> String {
        let mut md = format!(
            "# Claude.md — HarnessCode Project Context\n\n## Project: {}\n\n{}\n\n",
            self.name, self.description
        );

        if !self.conventions.is_empty() {
            md.push_str("## Conventions\n\n");
            for c in &self.conventions {
                md.push_str(&format!("- {}\n", c));
            }
        }

        if !self.restricted_paths.is_empty() {
            md.push_str("\n## Restricted Paths (do not modify)\n\n");
            for p in &self.restricted_paths {
                md.push_str(&format!("- `{}`\n", p));
            }
        }

        md
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_context_markdown_contains_task() {
        let mut ctx = AgentsContext::new("Refactor authentication module");
        ctx.add_step("Analyse existing auth.rs");
        ctx.add_step("Write unit tests");
        ctx.complete_step(1);

        let md = ctx.to_markdown();
        assert!(md.contains("Refactor authentication module"));
        assert!(md.contains("[x] Analyse existing auth.rs"));
        assert!(md.contains("[ ] Write unit tests"));
    }

    #[test]
    fn project_context_claude_md_contains_name() {
        let ctx = ProjectContext {
            name: "HarnessCode".to_string(),
            description: "Safe AI coding agent".to_string(),
            conventions: vec!["Use idiomatic Rust".to_string()],
            restricted_paths: vec!["src/auth.rs".to_string()],
        };

        let md = ctx.to_claude_md();
        assert!(md.contains("HarnessCode"));
        assert!(md.contains("Use idiomatic Rust"));
        assert!(md.contains("`src/auth.rs`"));
    }
}
