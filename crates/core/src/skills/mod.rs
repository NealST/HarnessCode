//! # Skills
//!
//! This module provides skill discovery, parsing, and registration.
//!
//! Skills are markdown files (SKILL.md) with optional YAML frontmatter that
//! extend the agent's capabilities.  They can be invoked:
//!
//! * **Explicitly** — by the user typing `/skill-name [args]`
//! * **Automatically** — by the model calling the `invoke_skill` tool when
//!   the skill's description matches the user's request
//!
//! ## Directory discovery order (lowest → highest priority)
//!
//! | Location | Priority |
//! |---|---|
//! | `<project>/.agents/skills/<name>/SKILL.md` | 1 (lowest) |
//! | `<project>/.claude/skills/<name>/SKILL.md` | 2 |
//! | `<project>/.harness/skills/<name>/SKILL.md` | 3 |
//! | `~/.harness/skills/<name>/SKILL.md` | 4 (highest) |

pub mod loader;
pub mod parser;

pub use loader::{SkillRegistry, SkillSummary};
pub use parser::{Skill, SkillParseError};
