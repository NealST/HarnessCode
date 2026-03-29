# AGENTS.md

## Project overview

**harnesscode-core** — Core engine and brain for HarnessCode: multi-agent orchestration, risk management, and context handling.

Languages / stack: Rust

## Project structure

```
core/
├── src/
│   ├── agents/
│   ├── context/
│   ├── controller/
│   ├── llm/
│   ├── observability/
│   ├── tools/
│   ├── config.rs
│   └── lib.rs
└── Cargo.toml
```

## Setup & build commands

- `cargo build`

## Testing instructions

- `cargo test`
- Fix any test failures before submitting your changes.

## Lint & formatting

- `cargo clippy`

## Code style & conventions

- Follow idiomatic Rust (clippy clean, `rustfmt` formatted).

---

## Agent pipeline

The controller runs a fixed six-stage pipeline for every user request.
Each stage is an `async_trait`-based agent that receives a JSON string, calls the LLM (optionally with tools), and returns `AgentOutput { role, summary, payload, success, tokens }`.

### Stage overview

```
Judge → Scoper → Planner → Conductor → Risk → Reviewer
```

Stages are sequential.  If any stage before Conductor fails the pipeline aborts.
Risk is non-fatal: if it fails or is skipped, the Reviewer runs with `risk_unavailable: true` in its context.

---

### Judge

**Role:** `AgentRole::Judge`
**Purpose:** Classifies the user's request and may rewrite it for cleaner downstream context.
**Input:** Raw user prompt + session context (conversation history, clarified facts, open questions).
**Output payload keys:**
- `needs_clarification: bool` — if `true`, the pipeline emits a `ClarificationNeeded` event and halts.
- `clarification_questions: Vec<String>` — questions to ask the user.
- `effective_request: String` — cleaned-up request forwarded to later stages.
- `is_drift: bool` — if `true`, the controller may emit a `DriftDetected` event.

---

### Scoper

**Role:** `AgentRole::Scoper`
**Purpose:** Identifies which files and areas of the codebase are relevant to the request.
**Input:** `effective_request` from Judge + `last_scope` from session memory.
**Output payload keys:**
- `files: Vec<String>` — file paths considered in scope.
- `summary: String` — plain-English scope description.

---

### Planner

**Role:** `AgentRole::Planner`
**Purpose:** Produces a step-by-step implementation plan.
**Input:** scope + effective_request + `last_plan` from session memory.
**Output payload keys:**
- `steps: Vec<{ title, description }>` — ordered action items.
- `summary: String` — one-sentence plan description.

---

### Conductor

**Role:** `AgentRole::Conductor`
**Purpose:** Implements the plan by running a ReAct tool loop (read/write/patch files, run commands, search).
**Tool loop return type:** `(String, TokenUsage, Vec<WrittenFile>)`

#### WrittenFile struct

```rust
pub struct WrittenFile {
    pub path: String,
    pub change_type: ChangeType,  // Write | Patch  (serialised as "write" | "patch")
    pub change: String,           // full content (Write) or diff text (Patch), truncated to 5 000 chars
}
```

**Output payload keys injected by conductor:**
- `affected_files: Vec<String>` — deduped, sorted list of paths touched.
- `files_changed: usize` — count of distinct paths.
- `actual_changes: Vec<{ path, change_type, change }>` — ground-truth record of every write/patch applied.

---

### Risk

**Role:** `AgentRole::Risk`
**Purpose:** Assesses the risk of the changes made by the Conductor. Non-fatal: failure produces `risk_unavailable: true`.

#### Input contract

The controller builds a slim payload containing:
```json
{
  "request":        "<effective request>",
  "affected_files": ["<path>", ...],
  "files_changed":  3,
  "diff":           "<unified diff or empty>",
  "explanation":    "<conductor summary>",
  "actual_changes": [
    { "path": "src/foo.rs", "change_type": "write", "change": "<content truncated to 5000 chars>" },
    { "path": "src/bar.rs", "change_type": "patch", "change": "<diff truncated to 5000 chars>" }
  ]
}
```

`actual_changes` is the primary signal; `diff` / `explanation` are fallbacks.

#### Skip guard

If both `actual_changes` is empty **and** `diff` is empty, the LLM call is bypassed and an `AgentOutput` with `risk_unavailable: true` is returned immediately.  The observability span is recorded as `SpanStatus::Skipped { reason: "no file changes recorded" }`.

#### Output payload keys

- `risk_level: "low" | "medium" | "high"` — only these three values are valid; anything else is normalised to `"unknown"`.
- `reason: String` — explanation of the assessed risk.
- `affected_areas: Vec<String>` — subsystems or layers affected.
- `breaking_change: bool` — whether a breaking change is likely.
- `security_implications: String` — free-text security notes.
- `cr_focus: String` — what reviewers should focus on.
- `risk_unavailable: bool` — `true` if the LLM call was skipped or failed.

#### RiskAssessed pipeline event

After `StageCompleted` for the Risk stage, the controller emits:
```rust
PipelineEvent::RiskAssessed {
    risk_level: String,
    reason: String,
    affected_areas: Vec<String>,
    breaking_change: bool,
    security_implications: String,
    cr_focus: String,
    risk_unavailable: bool,
}
```

The CLI renders this as a coloured block; the desktop `PipelineRunView` renders a `RiskCard`.

---

### Reviewer

**Role:** `AgentRole::Reviewer`
**Purpose:** Synthesises all stage outputs into a human-readable review summary.
**Input:** full pipeline outputs (Judge, Scoper, Planner, Conductor, Risk) + `risk_assessment` payload.
**Notable behaviour:** When `risk_unavailable: true` is present in `risk_assessment`, the reviewer acknowledges that risk data is unavailable instead of fabricating a verdict.
**Output payload keys:**
- `summary: String` — the text saved to session memory as the pipeline response.

---

## Session memory

`SessionMemory` is persisted per session in `.harness/memory/sessions/<id>.json`.

Key fields written after a successful pipeline run (via `SessionMemoryPatch`):
- `new_conversation_turn` — `{ request, response_summary, timestamp_secs }`
- `last_risk_level` — `"low" | "medium" | "high"` from the most recent assessed run, or unchanged if risk was unavailable.
- `last_scope` — Scoper payload.
- `last_plan` — Planner payload.

`last_risk_level` lets downstream agents (e.g. a future Scoper or Judge) adjust behaviour based on the risk trajectory of the session.

