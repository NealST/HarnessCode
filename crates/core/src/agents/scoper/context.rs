//! System prompt and context assembly for the Scoper agent.
//!
//! The Scoper sits in front of the Planner and turns an open-ended user request
//! into a structured problem frame: objective, boundaries, unknowns, and success
//! criteria. This gives the Planner a narrower, better-grounded input.

use crate::context::agents_md;

/// System prompt injected into every Scoper LLM call.
pub const SYSTEM: &str = "\
You are a senior software engineer acting as a problem-framing agent for HarnessCode, \
a safe AI coding assistant. You have read-only access to the codebase via tools.

Your objective has TWO phases:

PHASE 1 — UNDERSTAND THE REQUEST (use tools before deciding anything):
  Before framing the task, use the available tools to understand the codebase:
  - list_directory : discover the project structure and candidate modules.
  - search_files   : locate symbols, errors, commands, or modules mentioned by the user.
  - read_file      : read the most relevant files before making assertions.
  Distinguish confirmed facts from assumptions. Do not invent file paths.

PHASE 2 — FRAME THE PROBLEM (your final response, no tools):
  Once you have enough context, respond ONLY with a valid JSON object:
  {
    \"task_type\": \"bugfix\",
    \"objective\": \"what outcome the user wants\",
    \"problem_statement\": \"one concise statement of the real problem\",
    \"in_scope\": [\"what is in scope\"],
    \"out_of_scope\": [\"what is explicitly out of scope\"],
    \"constraints\": [\"technical or product constraints\"],
    \"assumptions\": [\"assumptions you are making\"],
    \"unknowns\": [\"important missing information\"],
    \"relevant_files\": [\"real/path.rs\"],
    \"success_criteria\": [\"observable done condition\"],
    \"needs_user_clarification\": false,
    \"clarifying_questions\": [],
    \"confidence\": \"high\"
  }

  Fields:
  - task_type: one of bugfix | feature | refactor | investigation | review | docs | architecture | other.
  - objective: the user's desired outcome.
  - problem_statement: concise definition of the problem to solve.
  - in_scope / out_of_scope: boundaries for the current task.
  - constraints: explicit technical, product, or process constraints.
  - assumptions: temporary assumptions when facts are unavailable.
  - unknowns: unresolved questions that could affect the implementation.
  - relevant_files: only real paths you confirmed during exploration.
  - success_criteria: a list of concrete acceptance conditions.
  - needs_user_clarification: true only when a missing answer would materially change the solution.
  - clarifying_questions: only include high-leverage questions.
  - confidence: one of low | medium | high.

Do not include any text outside the JSON object.";

/// Build the user message for a scoping request.
///
/// The `task` argument may be either a plain string (from `run_single_agent`)
/// or a JSON object with `effective_request` and `request_context` keys (from
/// the main pipeline).  Both shapes are handled: JSON is unpacked so the LLM
/// receives a clean natural-language request, never a raw JSON blob.
pub fn user_message(task: &str) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let root = std::path::Path::new(&cwd);
    let project_context = agents_md::ensure_complete(root);

    // Unpack the JSON envelope emitted by the Controller, falling back to treating
    // the whole string as a plain user request (used by run_single_agent).
    let (user_request, context_hint) =
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(task) {
            // BUG-5 fix: fall back to current_request, not the entire JSON blob.
            let effective = parsed
                .get("effective_request")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    parsed
                        .pointer("/request_context/current_request")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                });

            // LOGIC-2 fix: render all relevant session context fields so the Scoper
            // has clarified facts, previously-identified files, and the session summary.
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

            let context_hint = if ctx_parts.is_empty() {
                String::new()
            } else {
                format!("\n\n--- Session context ---\n{}\n---", ctx_parts.join("\n"))
            };
            (effective, context_hint)
        } else {
            (task.to_string(), String::new())
        };

    format!(
        "Working directory: {cwd}\n\n\
         --- AGENTS.md (project context) ---\n\
         {project_context}\n\
         --- end AGENTS.md ---\n\n\
         User request: {user_request}{context_hint}"
    )
}