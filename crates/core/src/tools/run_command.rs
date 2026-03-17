//! Actuator+Sensor tool: run a shell command and capture its output.
//!
//! # Security
//! Only whitelisted executables are allowed.  Arguments are passed directly
//! to `tokio::process::Command` (no shell expansion), so shell injection is
//! not possible.  The command is killed after a 60-second timeout.

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use async_trait::async_trait;
use std::time::Duration;

pub struct RunCommandTool;

/// Allowed top-level executables.  Only the first token of the command
/// (the program name, *without* path) is checked against this list.
const ALLOWED_PROGRAMS: &[&str] = &[
    "cargo",
    "rustup",
    "rustfmt",
    "clippy-driver",
    "git",
    "npm",
    "npx",
    "yarn",
    "pnpm",
    "node",
    "deno",
    "grep",
    "rg",      // ripgrep
    "find",
    "ls",
    "cat",
    "echo",
    "which",
    "test",    // POSIX test
    "wc",
    "head",
    "tail",
    "diff",
    "sort",
    "uniq",
    "jq",
];

/// Maximum combined stdout+stderr size returned to the LLM (100 KB).
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Timeout for each command invocation.
const TIMEOUT_SECS: u64 = 60;

#[async_trait]
impl Tool for RunCommandTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "run_command".into(),
            description: "Run a whitelisted shell command and return its stdout and stderr. \
                          The first word of `command` must be one of the allowed programs: \
                          cargo, git, npm, npx, yarn, pnpm, node, grep, rg, find, ls, etc. \
                          Dangerous programs (rm, sudo, curl | sh, …) are rejected. \
                          The command is killed after 60 seconds."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command string to execute, e.g. 'cargo test' or 'git status'."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the command. Defaults to '.'."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let command_str = match args["command"].as_str() {
            Some(c) => c.to_string(),
            None => return ToolResult::err("Missing required argument: command"),
        };
        let cwd = args["cwd"].as_str().unwrap_or(".").to_string();

        // Tokenise (simple split — no shell expansion needed).
        let tokens: Vec<&str> = command_str.split_whitespace().collect();
        if tokens.is_empty() {
            return ToolResult::err("Empty command");
        }
        let program = tokens[0];
        let prog_name = std::path::Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(program);

        if !ALLOWED_PROGRAMS.contains(&prog_name) {
            return ToolResult::err(format!(
                "Program '{}' is not in the allowed list. Allowed: {}",
                prog_name,
                ALLOWED_PROGRAMS.join(", ")
            ));
        }

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&tokens[1..]);
        cmd.current_dir(&cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Ensure the child is killed when the timeout fires and the future is dropped.
        cmd.kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("Failed to spawn '{}': {}", program, e)),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(TIMEOUT_SECS),
            child.wait_with_output(),
        )
        .await;

        match result {
            Err(_) => ToolResult::err(format!(
                "Command '{}' timed out after {} seconds",
                command_str, TIMEOUT_SECS
            )),
            Ok(Err(e)) => ToolResult::err(format!("Command error: {}", e)),
            Ok(Ok(output)) => {
                let mut combined = String::new();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !stdout.is_empty() {
                    combined.push_str("=== stdout ===\n");
                    combined.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("=== stderr ===\n");
                    combined.push_str(&stderr);
                }

                if combined.len() > MAX_OUTPUT_BYTES {
                    combined.truncate(MAX_OUTPUT_BYTES);
                    combined.push_str("\n… (output truncated)");
                }

                let exit_code = output.status.code().unwrap_or(-1);
                if !output.status.success() {
                    let msg = format!("[exit {}]\n{}", exit_code, combined);
                    return ToolResult::err(msg);
                }

                if combined.is_empty() {
                    combined = format!("[exit {}] (no output)", exit_code);
                }
                ToolResult::ok(combined)
            }
        }
    }
}
