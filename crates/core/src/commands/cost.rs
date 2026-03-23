//! `/cost` — display turn count and estimated token usage for the current session.

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `cost`.
pub(super) fn parse(_t: &ParseTokens<'_>) -> BuiltinCommand {
    BuiltinCommand::Cost
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn cost_keyword() {
        assert_eq!(parse_builtin("/cost"), Some(BuiltinCommand::Cost));
    }
}
