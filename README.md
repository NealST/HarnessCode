# HarnessCode 🤖🔒

> **The first AI coding agent built on cybernetics and absolute safety.**

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75+-orange.svg)](https://www.rust-lang.org)

---

## Vision

HarnessCode is an **enterprise-grade, cybernetics-inspired AI coding assistant** that treats safety and
correctness as first-class citizens. It combines the rigour of control-theory feedback loops with the
power of modern large language models, giving teams a coding agent that is:

- **Safe by design** — every file-system write is risk-scored before execution.
- **Observable** — a cybernetic controller records every action, compares actual vs. expected
  outcomes, and corrects course automatically.
- **Token-efficient** — a multi-agent pipeline (Planner → Conductor → Reviewer) keeps context windows
  small and focused.
- **Dual-UX** — ships as a beautiful TUI/CLI _and_ as a native desktop app powered by Tauri v2 with
  a Generative UI (React + TypeScript + TailwindCSS).

---

## Architecture

### 架构分层图

```mermaid
graph TB
    subgraph UI["🖥️ UI 交互层 (Interaction Layer)"]
        CLI["CLI / TUI<br/>indicatif · inquire · crossterm"]
        TAURI["Tauri v2 Desktop<br/>React · TypeScript · TailwindCSS"]
    end

    subgraph ORCH["🎛️ Agent 调度 & 上下文管理层 (Orchestration Layer)"]
        CTRL["Controller<br/>TOTE 控制循环 / 自动重试"]
        RISK["Risk Manager<br/>文件安全评分 (Low · Medium · High)"]
        CTX["Context Manager<br/>project.md · agents.md"]
        CFG["Config System<br/>layered TOML · multi-profile"]
    end

    subgraph AGENTS["🤖 子 Agent 层 (Sub-Agent Layer)"]
        PLAN["🧠 Planner Agent<br/>任务拆解 & 步骤规划"]
        CODE["💻 Conductor Agent<br/>计划执行 & 变更应用"]
        REV["🔍 Reviewer Agent<br/>质量审查 & 通过判定"]
    end

    subgraph TOOLS["🛠️ 工具层 (MCP & Skills)"]
        MCP["MCP Servers<br/>Filesystem · Git · Shell · Browser"]
        SKILLS["Custom Skills<br/>代码分析 · 测试运行 · 安全扫描"]
    end

    subgraph MODELS["🧠 模型层 (Model Layer)"]
        OAI["OpenAI-Compatible<br/>GPT-4o · DeepSeek · Ollama · Groq · Azure"]
        ANT["Anthropic<br/>Claude 3.5 Sonnet · Claude 3 Opus"]
    end

    CLI -->|user task| CTRL
    TAURI -->|user task| CTRL
    CFG -.->|profile resolution| CTRL
    CTRL <-->|risk check| RISK
    CTRL <-->|context r/w| CTX
    CTRL -->|plan| PLAN
    CTRL -->|code| CODE
    CTRL -->|review| REV

    PLAN --> MCP
    CODE --> MCP
    REV --> MCP
    PLAN --> SKILLS
    CODE --> SKILLS
    REV --> SKILLS

    PLAN -->|LLM call| OAI
    PLAN -->|LLM call| ANT
    CODE -->|LLM call| OAI
    CODE -->|LLM call| ANT
    REV -->|LLM call| OAI
    REV -->|LLM call| ANT

    style UI    fill:#1a3a5c,stroke:#4a90d9,color:#e0e8f0
    style ORCH  fill:#1e3d1a,stroke:#5aad3c,color:#e0f0e0
    style AGENTS fill:#3d1a3d,stroke:#c050c0,color:#f0e0f0
    style TOOLS fill:#3d3d1a,stroke:#d0b030,color:#f0f0e0
    style MODELS fill:#3d1a1a,stroke:#d04040,color:#f0e0e0
```

---

### Codebase Overview

```
┌──────────────────────────────────────────────────────────┐
│                      HarnessCode                         │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │               crates/core  (Engine)              │    │
│  │                                                  │    │
│  │  ┌─────────┐  ┌─────────┐  ┌────────────────┐   │    │
│  │  │ Planner │→ │Conductor│→ │    Reviewer    │   │    │
│  │  └────┬────┘  └────┬────┘  └───────┬────────┘   │    │
│  │       └─────────────┴──────────────┘            │    │
│  │              Controller (feedback loop)          │    │
│  │                   │                              │    │
│  │          RiskManager (file safety)               │    │
│  └──────────────────────────────────────────────────┘    │
│                                                          │
│  ┌───────────────┐        ┌────────────────────────┐     │
│  │  crates/cli   │        │   apps/desktop         │     │
│  │  (TUI/CLI)    │        │   Tauri v2 + React     │     │
│  └───────────────┘        └────────────────────────┘     │
└──────────────────────────────────────────────────────────┘
```

### Cybernetics Control Loop

HarnessCode implements a classic **TOTE** (Test-Operate-Test-Exit) cybernetic loop:

1. **Test** — Planner evaluates the current codebase state against the desired goal.
2. **Operate** — Conductor executes the plan inside a sandboxed environment.
3. **Test** — Reviewer checks outputs (tests, linting, risk scores) against success criteria.
4. **Exit** — Loop terminates when criteria are met, or escalates to the human operator.

### Risk Management

Every file touched by HarnessCode is run through the `RiskManager`:

| Risk Level | Trigger                           | Action           |
| ---------- | --------------------------------- | ---------------- |
| `Low`      | General source files              | Auto-proceed     |
| `Medium`   | Config files, CI scripts          | Log a warning    |
| `High`     | Auth files, secrets, `Cargo.toml` | Block + ask user |

### Multi-Agent Collaboration

| Agent    | Responsibility                                                                |
| -------- | ----------------------------------------------------------------------------- |
| Planner  | Decomposes the task into atomic steps, maintains the `agents.md` context file |
| Conductor | Executes the plan — applies changes and runs commands in an isolated sandbox  |
| Reviewer | Runs tests, linting, security scans, and decides pass/fail                    |

---

## Workspace Structure

```
HarnessCode/
├── Cargo.toml              # Workspace root
├── README.md
│
├── crates/
│   ├── core/               # Library: shared engine & brain
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── agents/        # Agent trait + concrete implementations
│   │       ├── controller/    # Cybernetic Controller, guardrails, events
│   │       ├── tools/         # Tool registry and built-in tools
│   │       └── context/
│   │
│   └── cli/                # Binary: Terminal UI entry point
│       └── src/
│           └── main.rs
│
└── apps/
    └── desktop/            # Tauri v2 desktop application
        ├── package.json    # Frontend deps (React, TailwindCSS)
        ├── index.html
        ├── src/            # React + TypeScript frontend
        └── src-tauri/      # Rust backend (Tauri commands)
```

---

## Getting Started

### Prerequisites

- [Rust 1.75+](https://rustup.rs/)
- [Node.js 18+](https://nodejs.org/) _(for the desktop app)_
- [Tauri v2 prerequisites](https://tauri.app/start/prerequisites/)

### Run the CLI

```bash
cargo run -p harnesscode-cli
```

### Run the Desktop App

```bash
cd apps/desktop
npm install
npm run tauri dev
```

### Build Everything

```bash
cargo build --workspace
```

---

## Contributing

1. Fork the repository.
2. Create a feature branch: `git checkout -b feat/my-feature`.
3. Run `cargo clippy -- -D warnings` and `cargo test` before opening a PR.
4. Open a pull request and describe your changes.

---

## License

This project is licensed under the **MIT License** — see the [LICENSE](LICENSE) file for details.
