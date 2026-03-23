//! `/session` subcommands — list, switch, and delete saved sessions.
//!
//! | Syntax                     | Action                                       |
//! |----------------------------|----------------------------------------------|
//! | `/session list`            | Print all saved sessions.                    |
//! | `/session use [id]`        | Switch to `id`; caller shows picker if None. |
//! | `/session delete <id>`     | Permanently delete the named session.        |
//! | `/session rm <id>`         | Alias for `/session delete`.                 |

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `session` and dispatches to the appropriate subcommand.
pub(super) fn parse(t: &ParseTokens<'_>) -> BuiltinCommand {
    match t.arg1.map(|s| s.to_lowercase()).as_deref() {
        Some("list") => BuiltinCommand::SessionList,

        Some("use") => BuiltinCommand::SessionUse(t.arg2.map(str::to_string)),

        Some("delete") | Some("rm") => match t.arg2 {
            Some(id) => BuiltinCommand::SessionDelete(id.to_string()),
            None => BuiltinCommand::Unknown(
                "/session delete requires a session id".to_string(),
            ),
        },

        _ => BuiltinCommand::Unknown(
            "Unknown /session subcommand. Try /help".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn session_list() {
        assert_eq!(parse_builtin("/session list"), Some(BuiltinCommand::SessionList));
    }

    #[test]
    fn session_use_no_id() {
        assert_eq!(
            parse_builtin("/session use"),
            Some(BuiltinCommand::SessionUse(None))
        );
    }

    #[test]
    fn session_use_with_id() {
        assert_eq!(
            parse_builtin("/session use my-id"),
            Some(BuiltinCommand::SessionUse(Some("my-id".to_string())))
        );
    }

    #[test]
    fn session_delete() {
        assert_eq!(
            parse_builtin("/session delete my-id"),
            Some(BuiltinCommand::SessionDelete("my-id".to_string()))
        );
    }

    #[test]
    fn session_rm_alias() {
        assert_eq!(
            parse_builtin("/session rm my-id"),
            Some(BuiltinCommand::SessionDelete("my-id".to_string()))
        );
    }

    #[test]
    fn session_delete_missing_id_is_unknown() {
        assert!(matches!(
            parse_builtin("/session delete"),
            Some(BuiltinCommand::Unknown(_))
        ));
    }

    #[test]
    fn unknown_session_subcommand() {
        assert!(matches!(
            parse_builtin("/session foobar"),
            Some(BuiltinCommand::Unknown(_))
        ));
    }
}
