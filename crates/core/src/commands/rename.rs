//! `/rename [name…]` — rename the current session.
//!
//! Everything after the `rename` keyword is captured verbatim as the new title,
//! so multi-word names like `/rename My Long Session` are supported.
//! Omitting the name (`/rename`) signals to the caller that it should prompt.

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `rename`.
///
/// Extracts the full remainder after `"rename"` from `without_slash` to
/// preserve multi-word names; passes `None` when no name is provided.
pub(super) fn parse(t: &ParseTokens<'_>) -> BuiltinCommand {
    // `without_slash` is e.g. `"rename My Long Session"`.
    // Slicing past the command keyword length captures `" My Long Session"`,
    // which `.trim()` reduces to `"My Long Session"`.
    let full_name = t.without_slash[t.cmd.len()..].trim();
    BuiltinCommand::Rename(if full_name.is_empty() {
        None
    } else {
        Some(full_name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn rename_single_word() {
        assert_eq!(
            parse_builtin("/rename mysession"),
            Some(BuiltinCommand::Rename(Some("mysession".to_string())))
        );
    }

    #[test]
    fn rename_multi_word() {
        assert_eq!(
            parse_builtin("/rename My Long Session Name"),
            Some(BuiltinCommand::Rename(Some("My Long Session Name".to_string())))
        );
    }

    #[test]
    fn rename_no_arg_is_none() {
        assert_eq!(parse_builtin("/rename"), Some(BuiltinCommand::Rename(None)));
    }

    #[test]
    fn rename_whitespace_only_is_none() {
        assert_eq!(
            parse_builtin("/rename   "),
            Some(BuiltinCommand::Rename(None))
        );
    }
}
