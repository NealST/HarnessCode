//! SKILL.md file parser.
//!
//! Parses a SKILL.md file that optionally starts with YAML frontmatter
//! delimited by `---` markers, followed by markdown body content.
//!
//! ## Frontmatter fields supported
//!
//! | Field                      | Type    | Default |
//! |----------------------------|---------|---------|
//! | `name`                     | string  | dir name |
//! | `description`              | string  | first paragraph of body |
//! | `disable_model_invocation` | bool    | false |
//! | `user_invocable`           | bool    | true |
//! | `argument_hint`            | string  | — |

use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SkillParseError {
    #[error("failed to read skill file at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("failed to parse YAML frontmatter in {path}: {source}")]
    Yaml { path: PathBuf, source: serde_yaml::Error },
}

// ── Frontmatter ───────────────────────────────────────────────────────────────

/// Raw YAML frontmatter fields (all optional).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    pub disable_model_invocation: bool,
    #[serde(default = "default_true", rename = "user-invocable")]
    pub user_invocable: bool,
    #[serde(rename = "argument-hint")]
    pub argument_hint: Option<String>,
}

fn default_true() -> bool {
    true
}

// ── Skill ─────────────────────────────────────────────────────────────────────

/// A fully parsed skill, ready to be registered and invoked.
#[derive(Debug, Clone)]
pub struct Skill {
    /// The skill's slash-command name (lowercase, hyphens OK).
    pub name: String,
    /// Human-readable description; shown in autocomplete and injected into the
    /// model's context so it knows when to invoke this skill.
    pub description: String,
    /// When `true`, the model cannot invoke this skill automatically via the
    /// `invoke_skill` tool; only explicit `/name` invocations work.
    pub disable_model_invocation: bool,
    /// When `false`, the skill is hidden from the `/` autocomplete menu.
    pub user_invocable: bool,
    /// Optional hint shown in the autocomplete UI (e.g. `"[issue-number]"`).
    pub argument_hint: Option<String>,
    /// Markdown body of the skill (the part after the frontmatter).
    pub body: String,
    /// Absolute path to the skill's directory (where SKILL.md lives).
    pub dir: PathBuf,
}

impl Skill {
    /// Render the skill body with argument substitution.
    ///
    /// Supported substitutions:
    /// * `$ARGUMENTS`     → the full args string
    /// * `$ARGUMENTS[N]`  → the N-th whitespace-separated argument (0-based)
    /// * `$N`             → shorthand for `$ARGUMENTS[N]`
    ///
    /// If `$ARGUMENTS` is not present in the body, the full args string is
    /// appended as `\nARGUMENTS: <args>` — matching Claude Code's behaviour.
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        let arg_list: Vec<&str> = args.split_whitespace().collect();

        let mut out = self.body.clone();

        // Detect whether the body uses any argument placeholders *before*
        // substitution, so we know whether to append ARGUMENTS: later.
        let had_arguments_placeholder = out.contains("$ARGUMENTS");

        // Detect positional placeholders ($N or $ARGUMENTS[N]) in the
        // original body, independent of how many args were actually supplied.
        let positional_re = Regex::new(r"\$ARGUMENTS\[\d+\]|\$\d+").unwrap();
        let had_positional = positional_re.is_match(&out);

        // Replace $ARGUMENTS[N] and $N in a **single pass** to avoid partial
        // matches (e.g. `$1` incorrectly matching inside `$10`).
        //
        // The alternation tries $ARGUMENTS[N] first (left-to-right priority),
        // so `$ARGUMENTS[0]` is matched before the bare `$0` branch would.
        let sub_re = Regex::new(r"\$ARGUMENTS\[(\d+)\]|\$(\d+)").unwrap();
        out = sub_re
            .replace_all(&out, |caps: &regex::Captures| {
                // Group 1 = index from $ARGUMENTS[N], group 2 = index from $N.
                let idx_str = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str())
                    .unwrap_or("0");
                let i: usize = idx_str.parse().unwrap_or(usize::MAX);
                // If the index is out of range, leave the placeholder intact.
                arg_list
                    .get(i)
                    .copied()
                    .unwrap_or_else(|| caps.get(0).map(|m| m.as_str()).unwrap_or(""))
                    .to_string()
            })
            .into_owned();

        // Replace $ARGUMENTS with the full argument string.
        out = out.replace("$ARGUMENTS", args);

        // Replace ${CLAUDE_SKILL_DIR} with the skill's own directory path so
        // scripts bundled with the skill can be referenced portably.
        let skill_dir = self.dir.to_string_lossy();
        out = out.replace("${CLAUDE_SKILL_DIR}", &*skill_dir);

        // If no placeholder was present in the original body and args were
        // provided, append them as a labelled block (Claude Code behaviour).
        if !had_arguments_placeholder && !had_positional && !args.is_empty() {
            out.push_str(&format!("\n\nARGUMENTS: {args}"));
        }

        out
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Parse a `SKILL.md` file.
///
/// `dir_name` is used as the fallback skill name when the frontmatter omits
/// the `name` field.
pub fn parse_skill_file(path: &Path, dir_name: &str) -> Result<Skill, SkillParseError> {
    let raw = std::fs::read_to_string(path).map_err(|e| SkillParseError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let (frontmatter, body) = split_frontmatter(&raw);
    let fm: SkillFrontmatter = if let Some(yaml) = frontmatter {
        serde_yaml::from_str(yaml).map_err(|e| SkillParseError::Yaml {
            path: path.to_path_buf(),
            source: e,
        })?
    } else {
        SkillFrontmatter::default()
    };

    let name = fm
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| dir_name.to_string());

    let description = fm
        .description
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| first_paragraph(body));

    let dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    Ok(Skill {
        name,
        description,
        disable_model_invocation: fm.disable_model_invocation,
        user_invocable: fm.user_invocable,
        argument_hint: fm.argument_hint,
        body: body.trim().to_string(),
        dir,
    })
}

/// Split raw SKILL.md content into optional YAML frontmatter and body.
///
/// Returns `(Some(yaml_str), body_str)` when the file starts with `---`.
fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let raw = raw.trim_start_matches('\u{feff}'); // strip BOM if present
    if !raw.starts_with("---") {
        return (None, raw);
    }
    // Find the closing `---`
    let after_open = &raw[3..];
    if let Some(end) = after_open.find("\n---") {
        let yaml = after_open[..end].trim();
        let body = &after_open[end + 4..];
        // Skip an optional newline right after the closing ---
        let body = body.trim_start_matches('\n');
        (Some(yaml), body)
    } else {
        // No closing marker — treat whole file as body
        (None, raw)
    }
}

/// Extract the first non-empty paragraph from a markdown string (for use as
/// the fallback description).
fn first_paragraph(text: &str) -> String {
    let text = text.trim();
    // Strip leading heading markers
    let text = text.trim_start_matches('#').trim();
    // Take up to the first blank line
    let para: String = text
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let para = para.trim().to_string();
    if para.is_empty() {
        "No description provided.".to_string()
    } else {
        // Limit to ~200 chars to keep tool descriptions concise
        if para.len() > 200 {
            format!("{}…", &para[..197])
        } else {
            para
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_with_yaml() {
        let raw = "---\nname: my-skill\ndescription: Does stuff\n---\n\nBody here.\n";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm, Some("name: my-skill\ndescription: Does stuff"));
        assert_eq!(body.trim(), "Body here.");
    }

    #[test]
    fn split_frontmatter_without_yaml() {
        let raw = "# My Skill\n\nDoes stuff.";
        let (fm, body) = split_frontmatter(raw);
        assert!(fm.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn render_arguments_substitution() {
        let skill = Skill {
            name: "test".into(),
            description: "desc".into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            body: "Fix issue $ARGUMENTS in the codebase.".into(),
            dir: PathBuf::from("/tmp"),
        };
        assert_eq!(skill.render("123"), "Fix issue 123 in the codebase.");
    }

    #[test]
    fn render_positional_substitution() {
        let skill = Skill {
            name: "test".into(),
            description: "desc".into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            body: "Migrate $0 from $1 to $2.".into(),
            dir: PathBuf::from("/tmp"),
        };
        assert_eq!(skill.render("SearchBar React Vue"), "Migrate SearchBar from React to Vue.");
    }

    #[test]
    fn render_no_partial_match_on_two_digit_index() {
        // Body uses $10 and $1 — $1 must NOT match inside $10.
        let skill = Skill {
            name: "test".into(),
            description: "desc".into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            body: "first=$1 tenth=$10".into(),
            dir: PathBuf::from("/tmp"),
        };
        // 11 args: 0=a, 1=b, …, 10=k
        let rendered = skill.render("a b c d e f g h i j k");
        assert_eq!(rendered, "first=b tenth=k");
    }

    #[test]
    fn render_skill_dir_substitution() {
        let skill = Skill {
            name: "test".into(),
            description: "desc".into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            body: "Run ${CLAUDE_SKILL_DIR}/scripts/run.sh".into(),
            dir: PathBuf::from("/home/user/.harness/skills/my-skill"),
        };
        let rendered = skill.render("");
        assert_eq!(rendered, "Run /home/user/.harness/skills/my-skill/scripts/run.sh");
    }

    #[test]
    fn render_appends_when_no_placeholder() {
        let skill = Skill {
            name: "test".into(),
            description: "desc".into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            body: "Do the thing.".into(),
            dir: PathBuf::from("/tmp"),
        };
        let rendered = skill.render("extra info");
        assert!(rendered.contains("ARGUMENTS: extra info"));
    }
}
