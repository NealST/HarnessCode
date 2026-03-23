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

mod clear;
mod cost;
mod exit;
mod help;
mod rename;
mod session;

pub use help::help_text;

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
    /// The input looked like a command but wasn't recognised.
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

    // Split into at most 3 tokens: command name, arg1, remainder.
    let mut tokens = without_slash.splitn(3, ' ');
    let cmd_owned = tokens.next().unwrap_or("").to_lowercase();
    let arg1 = tokens.next().map(str::trim).filter(|s| !s.is_empty());
    let arg2 = tokens.next().map(str::trim).filter(|s| !s.is_empty());

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
        other => BuiltinCommand::Unknown(format!(
            "Unknown command: /{other}. Type /help for available commands."
        )),
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
    fn unknown_command() {
        assert!(matches!(
            parse_builtin("/foobar"),
            Some(BuiltinCommand::Unknown(_))
        ));
    }
}

