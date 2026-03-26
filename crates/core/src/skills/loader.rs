//! Skill discovery — scans multiple directory trees for SKILL.md files and
//! assembles a [`SkillRegistry`].
//!
//! ## Discovery order (lowest → highest priority)
//!
//! Lower-priority skills are loaded first; higher-priority entries with the
//! same name overwrite them.
//!
//! 1. `<project>/.agents/skills/<name>/SKILL.md`   — legacy compat
//! 2. `<project>/.claude/skills/<name>/SKILL.md`   — claude compat
//! 3. `<project>/.harness/skills/<name>/SKILL.md`  — primary project-level
//! 4. `~/.harness/skills/<name>/SKILL.md`           — primary home-level

use super::parser::{parse_skill_file, Skill, SkillParseError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

// ── SkillRegistry ─────────────────────────────────────────────────────────────

/// Runtime registry of all discovered skills.
///
/// Built once at startup (or on demand) by scanning skill directories.
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    /// Map from lowercase skill name → Skill.
    skills: HashMap<String, Skill>,
}

/// A lightweight summary of a skill, suitable for serialisation to the frontend.
#[derive(Debug, Clone)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
}

impl SkillRegistry {
    /// Load all skills discoverable from `project_root`, honouring priority order.
    pub fn load(project_root: &Path) -> Self {
        let mut registry = Self::default();

        // Build search paths in ascending priority order so that higher-priority
        // entries naturally overwrite lower-priority ones.
        let search_dirs: Vec<PathBuf> = vec![
            // 1. legacy .agents/skills (lowest priority)
            project_root.join(".agents").join("skills"),
            // 2. .claude/skills compat
            project_root.join(".claude").join("skills"),
            // 3. primary project-level
            project_root.join(".harness").join("skills"),
        ];

        for dir in &search_dirs {
            registry.load_from_dir(dir);
        }

        // 4. Home-level (highest priority — overwrites project skills of same name)
        if let Some(home) = dirs::home_dir() {
            let home_dir = home.join(".harness").join("skills");
            registry.load_from_dir(&home_dir);
        }

        debug!(
            count = registry.skills.len(),
            "SkillRegistry loaded"
        );
        registry
    }

    /// Scan a single `skills/` directory for `<name>/SKILL.md` entries.
    fn load_from_dir(&mut self, skills_dir: &Path) {
        if !skills_dir.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(skills_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!("Could not read skills dir {:?}: {e}", skills_dir);
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();

            match parse_skill_file(&skill_md, &dir_name) {
                Ok(skill) => {
                    debug!(name = %skill.name, path = ?skill_md, "Loaded skill");
                    // Normalise key to lowercase so that get() (which also
                    // lowercases) reliably finds skills regardless of the
                    // casing used in the frontmatter `name` field.
                    self.skills.insert(skill.name.to_lowercase(), skill);
                }
                Err(SkillParseError::Io { path, source }) => {
                    warn!("Could not read skill file {path:?}: {source}");
                }
                Err(SkillParseError::Yaml { path, source }) => {
                    warn!("Bad frontmatter in {path:?}: {source}");
                }
            }
        }
    }

    // ── Query API ──────────────────────────────────────────────────────────────

    /// Look up a skill by exact name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(&name.to_lowercase())
    }

    /// All skills that the user may invoke via `/name` (i.e. `user_invocable`).
    pub fn list_user_invocable(&self) -> Vec<&Skill> {
        let mut skills: Vec<_> = self
            .skills
            .values()
            .filter(|s| s.user_invocable)
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// All skills that the model may invoke via the `invoke_skill` tool (i.e.
    /// **not** `disable_model_invocation`).
    pub fn list_model_invocable(&self) -> Vec<&Skill> {
        let mut skills: Vec<_> = self
            .skills
            .values()
            .filter(|s| !s.disable_model_invocation)
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Summarised list for all user-invocable skills (for the frontend).
    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.list_user_invocable()
            .into_iter()
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
                argument_hint: s.argument_hint.clone(),
                user_invocable: s.user_invocable,
                disable_model_invocation: s.disable_model_invocation,
            })
            .collect()
    }

    /// Build the description fragment injected into the `invoke_skill` tool
    /// definition so the model knows which skills it can call.
    pub fn model_tool_description(&self) -> String {
        let invocable = self.list_model_invocable();
        if invocable.is_empty() {
            return "No skills are currently available.".to_string();
        }
        let lines: Vec<String> = invocable
            .iter()
            .map(|s| format!("  • {} — {}", s.name, s.description))
            .collect();
        lines.join("\n")
    }

    /// Whether any skills are loaded.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_skill_dir(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn loads_skills_from_harness_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".harness").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        make_skill_dir(
            &skills_dir,
            "my-skill",
            "---\nname: my-skill\ndescription: Does things\n---\n\nBody.\n",
        );

        let registry = SkillRegistry::load(tmp.path());
        assert!(registry.get("my-skill").is_some());
        assert_eq!(registry.get("my-skill").unwrap().description, "Does things");
    }

    #[test]
    fn higher_priority_dir_overwrites_lower() {
        let tmp = tempfile::tempdir().unwrap();

        // Lower priority: .agents/skills
        let agents_dir = tmp.path().join(".agents").join("skills");
        fs::create_dir_all(&agents_dir).unwrap();
        make_skill_dir(
            &agents_dir,
            "shared",
            "---\ndescription: from agents\n---\nBody.",
        );

        // Higher priority: .harness/skills
        let harness_dir = tmp.path().join(".harness").join("skills");
        fs::create_dir_all(&harness_dir).unwrap();
        make_skill_dir(
            &harness_dir,
            "shared",
            "---\ndescription: from harness\n---\nBody.",
        );

        let registry = SkillRegistry::load(tmp.path());
        assert_eq!(
            registry.get("shared").unwrap().description,
            "from harness"
        );
    }

    #[test]
    fn uppercase_name_in_frontmatter_is_found_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".harness").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // name field uses mixed case — should still be discoverable
        make_skill_dir(
            &skills_dir,
            "my-skill",
            "---\nname: MySkill\ndescription: Test\n---\nBody.",
        );
        let registry = SkillRegistry::load(tmp.path());
        // Lookup must succeed regardless of case supplied to get()
        assert!(registry.get("myskill").is_some());
        assert!(registry.get("MYSKILL").is_some());
        assert!(registry.get("MySkill").is_some());
    }

    #[test]
    fn empty_dir_returns_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(tmp.path());
        assert!(registry.is_empty());
    }
}
