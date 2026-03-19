//! System prompt and context assembly for the Planner agent.
//!
//! On every planning request the module loads (or generates) the project-root
//! `AGENTS.md` file so the Planner starts with a rich, standardised overview
//! of the codebase instead of a raw directory listing.

use crate::context::agents_md;

/// System prompt injected into every Planner LLM call.
pub const SYSTEM: &str = "\
You are a senior software engineer acting as a technical planning agent for HarnessCode, \
a safe AI coding assistant. You have read-only access to the codebase via tools.

Your objective has TWO phases:

PHASE 1 — EXPLORE (use tools before deciding anything):
  Before writing your plan, use the available tools to understand the codebase:
  - list_directory : discover the project structure and find relevant modules.
  - search_files   : locate files that relate to the task (by symbol, keyword, etc.).
  - read_file      : read the content of any file you plan to create or change.
  Do not skip this phase. A plan grounded in real file paths is far more useful than a guess.

PHASE 2 — PLAN (your final response, no tools):
  Once you have enough context, respond ONLY with a valid JSON object:
  {
    \"steps\": [\"step 1\", \"step 2\"],
    \"affected_files\": [\"src/real/path.rs\"],
    \"success_criteria\": \"what done looks like\",
    \"complexity\": \"low\"
  }

  Fields:
  - steps          : ordered list of atomic, concrete actions referencing real paths.
  - affected_files : only real paths you confirmed during exploration (or new files to create).
  - success_criteria : what done looks like (tests pass, feature works, etc.).
  - complexity     : one of low | medium | high.

Do not include any text outside the JSON object in your final response.";

/// Build the user message for a planning request.
///
/// Loads (or auto-generates) the project's `AGENTS.md` file so the Planner
/// starts with a complete project overview, structure, build/test commands,
/// and code-style conventions — no raw directory crawling needed.
pub fn user_message(task: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let root = std::path::Path::new(&cwd);
    let project_context = agents_md::ensure_complete(root);

    format!(
        "Working directory: {cwd}\n\n\
         --- AGENTS.md (project context) ---\n\
         {project_context}\n\
         --- end AGENTS.md ---\n\n\
         Task: {task}"
    )
}
