//! `/exit` and `/quit` — terminate the REPL / application.

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `exit` or `quit`.
pub(super) fn parse(_t: &ParseTokens<'_>) -> BuiltinCommand {
    BuiltinCommand::Exit
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn exit_keyword() {
        assert_eq!(parse_builtin("/exit"), Some(BuiltinCommand::Exit));
    }

    #[test]
    fn quit_alias() {
        assert_eq!(parse_builtin("/quit"), Some(BuiltinCommand::Exit));
    }
}
