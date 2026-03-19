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

