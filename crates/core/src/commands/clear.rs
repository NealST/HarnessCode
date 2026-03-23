//! `/clear` and `/reset` — wipe the current session's conversation history.

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `clear` or `reset`.
pub(super) fn parse(_t: &ParseTokens<'_>) -> BuiltinCommand {
    BuiltinCommand::Clear
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn clear_keyword() {
        assert_eq!(parse_builtin("/clear"), Some(BuiltinCommand::Clear));
    }

    #[test]
    fn reset_alias() {
        assert_eq!(parse_builtin("/reset"), Some(BuiltinCommand::Clear));
    }
}
