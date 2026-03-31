//! `/remember [query]` — recall past solutions from long-term memory.

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `remember`.
pub(super) fn parse(t: &ParseTokens<'_>) -> BuiltinCommand {
    // Everything after `/remember` (trimmed) is the optional query.
    let query = t
        .without_slash
        .strip_prefix("remember")
        .map(|rest| rest.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    BuiltinCommand::Remember { query }
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn remember_no_query() {
        assert_eq!(
            parse_builtin("/remember"),
            Some(BuiltinCommand::Remember { query: None })
        );
    }

    #[test]
    fn remember_with_query() {
        assert_eq!(
            parse_builtin("/remember how to cancel a pipeline"),
            Some(BuiltinCommand::Remember {
                query: Some("how to cancel a pipeline".into())
            })
        );
    }
}
