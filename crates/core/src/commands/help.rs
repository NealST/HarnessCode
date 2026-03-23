//! `/help` and `/?` — display the built-in command reference.

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `help` or `?`.
pub(super) fn parse(_t: &ParseTokens<'_>) -> BuiltinCommand {
    BuiltinCommand::Help
}

/// Returns the formatted built-in command reference as a `String`.
///
/// Uses ANSI escape codes for colour.  Callers may strip them when writing
/// to non-TTY outputs.
pub fn help_text() -> String {
    "\n  \x1b[1mBuilt-in commands\x1b[0m\n\
     \n\
     \x1b[36m  /help\x1b[0m                    Show this help\n\
     \x1b[36m  /session list\x1b[0m             List all saved sessions\n\
     \x1b[36m  /session use [id]\x1b[0m         Switch sessions (interactive if no id given)\n\
     \x1b[36m  /session delete <id>\x1b[0m      Permanently delete a session\n\
     \x1b[36m  /rename [name]\x1b[0m            Rename current session (multi-word names supported)\n\
     \x1b[36m  /clear\x1b[0m                    Clear conversation history for current session\n\
     \x1b[36m  /cost\x1b[0m                     Show turn count and estimated token usage\n\
     \x1b[36m  /exit\x1b[0m  \x1b[36m/quit\x1b[0m             Exit HarnessCode\n\
     \n\
     \x1b[2m  Any other input is sent to the AI pipeline.\x1b[0m\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn help_keyword() {
        assert_eq!(parse_builtin("/help"), Some(BuiltinCommand::Help));
    }

    #[test]
    fn question_mark_alias() {
        assert_eq!(parse_builtin("/?"), Some(BuiltinCommand::Help));
    }

    #[test]
    fn help_text_is_non_empty() {
        assert!(!super::help_text().is_empty());
    }
}
