//! `/init` — generate or update the project's `AGENTS.md` context file.
//!
//! Calls [`harnesscode_core::context::agents_md::generate`] for the supplied
//! project root and returns the rendered Markdown content.
//!
//! Writing the file and handling overwrite confirmation is intentionally left to
//! the caller (CLI, desktop, etc.) so that this module remains I/O-free and
//! easily testable.

use std::path::Path;

use crate::context::agents_md;

use super::{BuiltinCommand, ParseTokens};

/// Route handler: matches `init`.
pub(super) fn parse(_t: &ParseTokens<'_>) -> BuiltinCommand {
    BuiltinCommand::Init
}

/// Generate the `AGENTS.md` content for the project rooted at `root`.
///
/// Returns the rendered Markdown string.  The caller is responsible for
/// deciding whether to overwrite an existing file and for the actual write.
pub fn generate_agents_md(root: &Path) -> String {
    agents_md::generate(root)
}

#[cfg(test)]
mod tests {
    use crate::commands::{parse_builtin, BuiltinCommand};

    #[test]
    fn init_keyword() {
        assert_eq!(parse_builtin("/init"), Some(BuiltinCommand::Init));
    }

    #[test]
    fn generate_returns_non_empty() {
        let root = std::env::current_dir().unwrap();
        let content = super::generate_agents_md(&root);
        assert!(!content.is_empty());
        assert!(content.contains("AGENTS.md") || content.contains("Project"));
    }
}
