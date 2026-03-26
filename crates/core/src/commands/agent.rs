//! `/scope` and `/compact` — run a single built-in sub-agent directly.
//!
//! | Command              | What it does                                    |
//! |----------------------|-------------------------------------------------|
//! | `/scope <task>`      | Run the Scoper agent standalone on `<task>`     |
//! | `/compact`           | Run the Compactor against the current session   |

use super::{BuiltinAgentKind, BuiltinCommand, ParseTokens};

/// Route handler for `scope` and `compact`.
pub(super) fn parse(t: &ParseTokens<'_>) -> BuiltinCommand {
    match t.cmd {
        "scope" => BuiltinCommand::RunAgent {
            agent: BuiltinAgentKind::Scoper,
            args: t.without_slash["scope".len()..].trim_start().to_string(),
        },
        "compact" => BuiltinCommand::RunAgent {
            agent: BuiltinAgentKind::Compactor,
            args: String::new(),
        },
        _ => unreachable!("agent::parse called with unexpected cmd: {}", t.cmd),
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinAgentKind, BuiltinCommand};

    #[test]
    fn scope_with_args() {
        assert_eq!(
            parse_builtin("/scope refactor the auth module"),
            Some(BuiltinCommand::RunAgent {
                agent: BuiltinAgentKind::Scoper,
                args: "refactor the auth module".to_string(),
            })
        );
    }

    #[test]
    fn scope_no_args() {
        assert_eq!(
            parse_builtin("/scope"),
            Some(BuiltinCommand::RunAgent {
                agent: BuiltinAgentKind::Scoper,
                args: String::new(),
            })
        );
    }

    #[test]
    fn compact_no_args() {
        assert_eq!(
            parse_builtin("/compact"),
            Some(BuiltinCommand::RunAgent {
                agent: BuiltinAgentKind::Compactor,
                args: String::new(),
            })
        );
    }
}
