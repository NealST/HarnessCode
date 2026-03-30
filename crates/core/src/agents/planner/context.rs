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

Your objective has TWO steps:

STEP 1 — EXPLORE (use tools before deciding anything):
  Before writing your plan, use the available tools to understand the codebase:
  - list_directory : discover the project structure and find relevant modules.
  - search_files   : locate files that relate to the task (by symbol, keyword, etc.).
  - read_file      : read the content of any file you plan to create or change.
  Do not skip this step. A plan grounded in real file paths is far more useful than a guess.

STEP 2 — PLAN (your final response, no tools):
  Once you have enough context, decompose the work into sequential execution phases.
  Each phase should be independently executable and verifiable.
  Respond ONLY with a valid JSON object in this exact shape:
  {
    \"phases\": [
      {
        \"phase_id\": 1,
        \"title\": \"short human-readable title\",
        \"objective\": \"what this phase accomplishes\",
        \"steps\": [\"step a\", \"step b\"],
        \"affected_files\": [\"src/real/path.rs\"],
        \"success_criteria\": \"how to confirm this phase succeeded\",
        \"complexity\": \"low\"
      }
    ],
    \"global_success_criteria\": \"what the entire task looks like when fully done\"
  }

  Phase field rules:
  - phases         : ordered array of phases; at least one phase required.
  - phase_id       : 1-based integer, unique, sequential.
  - title          : ≤ 60 chars, describes what the phase does.
  - objective      : one sentence explaining the goal of this phase.
  - steps          : ordered list of atomic, concrete actions referencing real paths.
  - affected_files : high-confidence candidate set of files likely to be touched in THIS phase.
                     Include only real paths you confirmed during exploration (or new files to create).
                     This is planning guidance for the executor, not a hard whitelist.
  - success_criteria : what done looks like for THIS phase (e.g. 'cargo build passes', 'test X passes').
  - complexity     : one of low | medium | high.
  - global_success_criteria : overall acceptance criteria for the whole task.

Do not include any text outside the JSON object in your final response.";

/// Build the user message for a planning request.
///
/// Loads (or auto-generates) the project's `AGENTS.md` file so the Planner
/// starts with a complete project overview, structure, build/test commands,
/// and code-style conventions — no raw directory crawling needed.
///
/// The `task` argument is a JSON object with keys:
/// - `effective_request` — the Judge-resolved request (primary input)
/// - `user_request`      — the original raw prompt (shown when it differs from effective)
/// - `problem_frame`     — the Scoper's structured problem frame
/// - `request_context`   — full session context (history, clarified facts, known files)
pub fn user_message(task: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let root = std::path::Path::new(&cwd);
    let project_context = agents_md::ensure_complete(root);

    // Unpack the JSON envelope so the Planner receives clear labelled sections
    // rather than a raw JSON blob.
    let (effective_request, original_request_hint, problem_frame_section, session_context_section) =
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(task) {
            let req = parsed
                .get("effective_request")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(task)
                .to_string();

            // MISSING-2 fix: show the original user_request when it differs from effective_request
            // (e.g. after a drift-restart, effective_request becomes a reinforcement message).
            let original_hint = parsed
                .get("user_request")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && *s != req.as_str())
                .map(|s| format!("\nOriginal request (for reference): {s}"))
                .unwrap_or_default();

            let frame = if let Some(pf) = parsed.get("problem_frame") {
                format!(
                    "\n\n--- Problem frame (from Scoper) ---\n{}\n--- end problem frame ---",
                    serde_json::to_string_pretty(pf).unwrap_or_default()
                )
            } else {
                String::new()
            };

            // LOGIC-1 fix: render session context so the Planner knows about
            // clarified facts, previously-identified files, and prior-session summary.
            let rc = parsed.get("request_context");
            let ss = rc.and_then(|v| v.get("session_state"));

            let mut ctx_parts: Vec<String> = Vec::new();
            if let Some(s) = ss
                .and_then(|v| v.get("persistent_summary"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                ctx_parts.push(format!("Summary: {s}"));
            }
            if let Some(facts) = ss
                .and_then(|v| v.get("clarified_facts"))
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
            {
                let items: Vec<&str> = facts.iter().filter_map(|v| v.as_str()).collect();
                ctx_parts.push(format!("Clarified facts: {}", items.join("; ")));
            }
            if let Some(files) = ss
                .and_then(|v| v.get("known_relevant_files"))
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
            {
                let items: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
                ctx_parts.push(format!("Previously identified files: {}", items.join(", ")));
            }
            let session_ctx = if ctx_parts.is_empty() {
                String::new()
            } else {
                format!("\n\n--- Session context ---\n{}\n---", ctx_parts.join("\n"))
            };

            (req, original_hint, frame, session_ctx)
        } else {
            (task.to_string(), String::new(), String::new(), String::new())
        };

    format!(
        "Working directory: {cwd}\n\n\
         --- AGENTS.md (project context) ---\n\
         {project_context}\n\
         --- end AGENTS.md ---\n\n\
         Task: {effective_request}{original_request_hint}{session_context_section}{problem_frame_section}"
    )
}
