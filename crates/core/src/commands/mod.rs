//! # Built-in Command Definitions
//!
//! This module owns the command language shared by all HarnessCode interfaces
//! (CLI, future TUI, etc.).  It is intentionally pure: no I/O, no rendering,
//! no interactive prompts.  Callers receive a typed [`BuiltinCommand`] and are
//! free to handle it however suits their interface.
//!
//! Each built-in command lives in its own submodule:
//!
//! | Module | Commands |
//! |--------|----------|
//! | [`help`] | `/help`, `/?` |
//! | [`exit`] | `/exit`, `/quit` |
//! | [`clear`] | `/clear`, `/reset` |
//! | [`cost`] | `/cost` |
//! | [`rename`] | `/rename [name]` |
//! | [`session`] | `/session list|use|delete` |
//! | [`init`] | `/init` |

mod agent;
mod clear;
mod cost;
mod exit;
mod help;
mod init;
mod rename;
mod session;

pub use help::help_text;
pub use init::generate_agents_md;

/// Identifies which built-in sub-agent to invoke via [`BuiltinCommand::RunAgent`].
#[derive(Debug, Clone, PartialEq)]
pub enum BuiltinAgentKind {
    /// Scope / frame the task (Scoper agent).
    Scoper,
    /// Compact older conversation turns in the current session (Compactor agent).
    Compactor,
}

// ── Token bundle passed to each sub-parser ────────────────────────────────────

/// Tokenised representation of a `/command` string, shared by all sub-parsers.
///
/// Built once in [`parse_builtin`] and passed by reference to each sub-module's
/// `parse` function so that every parser sees a consistent, pre-split view of
/// the input without re-parsing.
pub(crate) struct ParseTokens<'a> {
    /// Lowercase first token (the command name), e.g. `"rename"`.
    pub cmd: &'a str,
    /// Second whitespace-separated token, if present.
    pub arg1: Option<&'a str>,
    /// Everything after the second token (the remainder), if present.
    pub arg2: Option<&'a str>,
    /// Full input after stripping the leading `/`, original case preserved.
    /// Needed by [`rename`] to capture multi-word names verbatim.
    pub without_slash: &'a str,
}

// ── Command enum ──────────────────────────────────────────────────────────────

/// A parsed built-in `/command` entered by the user.
#[derive(Debug, Clone, PartialEq)]
pub enum BuiltinCommand {
    /// Display the built-in command reference.
    Help,
    /// Exit the REPL / application.
    Exit,
    /// Clear the current session's conversation history.
    Clear,
    /// Show turn count and estimated token usage for the current session.
    Cost,
    /// Rename the current session.  `None` means the caller should prompt.
    Rename(Option<String>),
    /// List all saved sessions.
    SessionList,
    /// Switch to a session by id.  `None` means the caller should show a picker.
    SessionUse(Option<String>),
    /// Permanently delete the named session.
    SessionDelete(String),
    /// Generate or update the project's `AGENTS.md` context file.
    Init,
    /// Run a single built-in sub-agent directly, bypassing the full pipeline.
    ///
    /// `args` is the task string passed to the agent's `execute()` (may be empty
    /// for agents that derive their input from session state, e.g. Compactor).
    RunAgent { agent: BuiltinAgentKind, args: String },
    /// A potentially-valid skill slash-command.  Callers should check a
    /// [`SkillRegistry`](crate::skills::SkillRegistry) to confirm the skill
    /// exists; show the `Unknown` message if it does not.
    ///
    /// `name` is the raw (lowercase) command name without the leading `/`.
    /// `args` is everything that followed the command name (trimmed).
    InvokeSkill { name: String, args: String },
    /// The input looked like a command but wasn't recognised by builtins OR
    /// skills.  Callers should display this message.
    Unknown(String),
}

// ── Public parser ─────────────────────────────────────────────────────────────

/// Parse a user input string into a [`BuiltinCommand`], or return `None` if the
/// input is not a command (i.e. does not start with `/`).
///
/// ```
/// use harnesscode_core::commands::{parse_builtin, BuiltinCommand};
///
/// assert!(matches!(parse_builtin("hello"), None));
/// assert!(matches!(parse_builtin("/help"), Some(BuiltinCommand::Help)));
/// assert_eq!(
///     parse_builtin("/rename My Long Name"),
///     Some(BuiltinCommand::Rename(Some("My Long Name".into()))),
/// );
/// ```
pub fn parse_builtin(input: &str) -> Option<BuiltinCommand> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }
    let without_slash = &input[1..];

    // Split on runs of whitespace so that inputs with multiple consecutive
    // spaces (e.g. `/session  delete  id`) are tokenised correctly.
    let mut cmd_split = without_slash.splitn(2, |c: char| c.is_whitespace());
    let cmd_raw = cmd_split.next().unwrap_or("");
    let cmd_owned = cmd_raw.to_lowercase();

    // Everything after the command name, leading whitespace stripped.
    let after_cmd = cmd_split.next().unwrap_or("").trim_start();

    // Split the remainder into arg1 + arg2 (all further tokens collapsed into arg2).
    let mut arg_split = after_cmd.splitn(2, |c: char| c.is_whitespace());
    let arg1_str = arg_split.next().unwrap_or("");
    let arg1 = if arg1_str.is_empty() { None } else { Some(arg1_str) };
    let arg2_str = arg_split.next().unwrap_or("").trim_start();
    let arg2 = if arg2_str.is_empty() { None } else { Some(arg2_str) };

    let t = ParseTokens {
        cmd: &cmd_owned,
        arg1,
        arg2,
        without_slash,
    };

    Some(match t.cmd {
        "help" | "?"         => help::parse(&t),
        "exit" | "quit"      => exit::parse(&t),
        "clear" | "reset"    => clear::parse(&t),
        "cost"               => cost::parse(&t),
        "rename"             => rename::parse(&t),
        "session"            => session::parse(&t),
        "init"               => init::parse(&t),
        "scope" | "compact" => agent::parse(&t),
        other => BuiltinCommand::InvokeSkill {
            name: other.to_string(),
            args: after_cmd.to_string(),
        },
    })
}

// ── Module-level tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_command_returns_none() {
        assert!(parse_builtin("hello world").is_none());
        assert!(parse_builtin("  any text  ").is_none());
    }

    #[test]
    fn unknown_command_becomes_invoke_skill() {
        // Unrecognised commands are treated as potential skill invocations.
        // Callers must check the SkillRegistry; if not found, show the Unknown message.
        assert!(matches!(
            parse_builtin("/foobar"),
            Some(BuiltinCommand::InvokeSkill { name, args })
            if name == "foobar" && args.is_empty()
        ));
    }

    #[test]
    fn invoke_skill_with_args() {
        assert!(matches!(
            parse_builtin("/my-skill arg1 arg2"),
            Some(BuiltinCommand::InvokeSkill { name, args })
            if name == "my-skill" && args == "arg1 arg2"
        ));
    }

    // ── Whitespace robustness ─────────────────────────────────────────────────

    #[test]
    fn double_space_between_cmd_and_subcommand() {
        // Extra space between "session" and "delete" must not break parsing.
        assert_eq!(
            parse_builtin("/session  delete  my-id"),
            Some(BuiltinCommand::SessionDelete("my-id".to_string())),
        );
    }

    #[test]
    fn double_space_session_list() {
        assert_eq!(
            parse_builtin("/session  list"),
            Some(BuiltinCommand::SessionList),
        );
    }

    #[test]
    fn double_space_session_use() {
        assert_eq!(
            parse_builtin("/session  use  my-id"),
            Some(BuiltinCommand::SessionUse(Some("my-id".to_string()))),
        );
    }

    #[test]
    fn leading_trailing_whitespace_ignored() {
        assert_eq!(parse_builtin("  /help  "), Some(BuiltinCommand::Help));
        assert_eq!(
            parse_builtin("  /rename  My Session  "),
            Some(BuiltinCommand::Rename(Some("My Session".to_string()))),
        );
    }
}

