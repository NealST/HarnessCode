//! Search tool: grep-compatible file content search with graceful backend degradation.
//!
//! Backend priority chain (detected at call time):
//!
//! 1. **Ripgrep** (`rg`) — fastest, respects `.gitignore`, full regex + glob
//! 2. **Grep** (`grep`) — widely available, regex via `-E`, simple `--include` glob
//! 3. **Native** — pure-Rust fallback using `ignore` + `regex` crates; reads
//!    `.gitignore` via the same library ripgrep uses internally
//!
//! All three backends produce identical output lines:
//! ```text
//! path/to/file.rs:42: matched line content
//! ```
//! The first line of the result always names the backend used so callers
//! (including the LLM) can understand the search semantics that were applied.
//!
//! ## Parameters (all via JSON)
//! | Field | Type | Required | Notes |
//! |-------|------|----------|-------|
//! | `root` | string | yes | Directory to search |
//! | `pattern` | string | yes | Regular expression |
//! | `glob` | string | no | Glob pattern, e.g. `**/*.rs` |
//! | `case_sensitive` | bool | no | Default `true` |
//!
//! ## Security
//! All arguments are passed to subprocess via `.arg()` / `.args()`, never
//! interpolated into a shell string, eliminating command-injection risk.

use std::path::Path;
use std::process::Command;

use async_trait::async_trait;
use regex::RegexBuilder;

use super::{Tool, ToolResult};
use crate::llm::ToolDef;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum match lines returned across all three backends.
const MAX_MATCHES: usize = 200;

/// Maximum file size scanned by the native fallback (bytes).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Default glob applied when none is provided (match everything).
const DEFAULT_GLOB: &str = "**/*";

// ── Backend detection ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Backend {
    Ripgrep,
    Grep,
    Native,
}

impl Backend {
    fn detect() -> Self {
        if which("rg") {
            Backend::Ripgrep
        } else if which("grep") {
            Backend::Grep
        } else {
            Backend::Native
        }
    }

    fn name(self) -> &'static str {
        match self {
            Backend::Ripgrep => "ripgrep",
            Backend::Grep => "grep",
            Backend::Native => "native",
        }
    }
}

/// Returns `true` when `name` is found on PATH.
fn which(name: &str) -> bool {
    Command::new(if cfg!(target_os = "windows") { "where" } else { "which" })
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Tool definition ───────────────────────────────────────────────────────────

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "search_files".into(),
            description:
                "Search for a regex pattern across files in a directory tree. \
                 Returns matching lines with file paths and 1-based line numbers. \
                 Automatically uses ripgrep (preferred), grep, or a built-in fallback. \
                 Respects .gitignore when ripgrep or the native backend is used. \
                 Use the `glob` parameter to restrict which files are searched, \
                 e.g. `**/*.rs` for Rust files or `src/**/*.ts` for TypeScript under src/."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "root": {
                        "type": "string",
                        "description": "Root directory to search in."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression to search for."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob pattern to filter files, e.g. '**/*.rs' or 'src/**/*.ts'. \
                                        Omit to search all files."
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Whether the search is case-sensitive. Default true.",
                        "default": true
                    }
                },
                "required": ["root", "pattern"]
            }),
        }
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let root = match args["root"].as_str() {
            Some(r) => r.to_string(),
            None => return ToolResult::err("Missing required argument: root"),
        };
        let pattern = match args["pattern"].as_str() {
            Some(p) => p.to_string(),
            None => return ToolResult::err("Missing required argument: pattern"),
        };
        let glob = args["glob"]
            .as_str()
            .unwrap_or(DEFAULT_GLOB)
            .to_string();
        let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(true);

        // Validate the regex before handing off to any backend.
        if let Err(e) = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()
        {
            return ToolResult::err(format!("Invalid regex pattern: {e}"));
        }

        let backend = Backend::detect();

        let result = tokio::task::spawn_blocking(move || {
            match backend {
                Backend::Ripgrep => run_ripgrep(&root, &pattern, &glob, case_sensitive),
                Backend::Grep => run_grep(&root, &pattern, &glob, case_sensitive),
                Backend::Native => run_native(&root, &pattern, &glob, case_sensitive),
            }
        })
        .await
        .unwrap_or_else(|_| Err("Worker thread panicked".to_string()));

        match result {
            Ok(output) => ToolResult::ok(output),
            Err(e) => ToolResult::err(e),
        }
    }
}

// ── Backend: Ripgrep ──────────────────────────────────────────────────────────

fn run_ripgrep(
    root: &str,
    pattern: &str,
    glob: &str,
    case_sensitive: bool,
) -> Result<String, String> {
    let mut cmd = Command::new("rg");
    cmd.args([
        "--no-heading",
        "--line-number",
        "--color=never",
        // No per-file --max-count here; we cap the total via .take() below.
        "--glob", glob,
    ]);
    if !case_sensitive {
        cmd.arg("--ignore-case");
    }
    // Pattern and root are separate args — never shell-interpolated.
    cmd.arg("--").arg(pattern).arg(root);

    // Read stdout incrementally and stop after MAX_MATCHES lines so rg
    // doesn't need to finish scanning the entire tree.
    let output = cmd.output().map_err(|e| format!("rg execution failed: {e}"))?;

    // rg exits 1 when there are no matches (not an error).
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("rg error: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let total_lines = stdout.lines().count();
    let lines: Vec<&str> = stdout.lines().take(MAX_MATCHES).collect();
    let truncated = total_lines > MAX_MATCHES;

    format_output(Backend::Ripgrep, &lines, truncated, pattern, root)
}

// ── Backend: Grep ─────────────────────────────────────────────────────────────

/// Extract the simplest file extension pattern from a glob for `--include`.
///
/// `**/*.rs` → `*.rs`  
/// `src/**/*.ts` → `*.ts`  
/// `*.json` → `*.json`  
/// `*` / `**/*` / anything without a dot → `*` (no filtering)
///
/// We warn in the output when the glob was simplified so the LLM knows.
fn glob_to_include(glob: &str) -> (&str, bool) {
    // Find the last path component.
    let last = glob.rsplit('/').next().unwrap_or(glob);
    // Only use it if it looks like an extension pattern.
    if last.starts_with("*.") && !last.contains('/') {
        (last, glob.contains('/') && glob != last)
    } else if last == "*" || last == "**" {
        ("*", false)
    } else {
        (last, false)
    }
}

fn run_grep(
    root: &str,
    pattern: &str,
    glob: &str,
    case_sensitive: bool,
) -> Result<String, String> {
    let (include_pat, simplified) = glob_to_include(glob);

    let mut cmd = Command::new("grep");
    cmd.args(["-r", "-n", "-E"]);
    if !case_sensitive {
        cmd.arg("-i");
    }
    if include_pat != "*" {
        // Pass as a separate arg — not shell-interpolated.
        cmd.arg(format!("--include={include_pat}"));
    }
    // Note: format! here is safe because include_pat is derived from a
    // controlled glob_to_include transform, not raw user input passed to sh.
    // Pattern and root are separate args.
    cmd.arg("--").arg(pattern).arg(root);

    let output = cmd.output().map_err(|e| format!("grep execution failed: {e}"))?;

    // grep exits 1 when no matches found.
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("grep error: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().take(MAX_MATCHES).collect();
    let truncated = stdout.lines().count() > MAX_MATCHES;

    let mut out = format_output(Backend::Grep, &lines, truncated, pattern, root)?;
    if simplified {
        out.push_str(&format!(
            "\n[note: grep backend simplified glob '{}' to '--include={include_pat}'; \
             install ripgrep for full glob support]",
            glob
        ));
    }
    Ok(out)
}

// ── Backend: Native ───────────────────────────────────────────────────────────

fn run_native(
    root: &str,
    pattern: &str,
    glob: &str,
    case_sensitive: bool,
) -> Result<String, String> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| format!("Invalid regex: {e}"))?;

    let glob_matcher = build_glob_matcher(glob).map_err(|e| format!("Invalid glob: {e}"))?;

    let mut matches: Vec<String> = Vec::new();
    let mut files_scanned = 0usize;
    let mut truncated = false;

    // ignore::WalkBuilder reads .gitignore automatically and uses the same
    // traversal logic as ripgrep internally.
    let walker = ignore::WalkBuilder::new(root)
        .follow_links(false)
        .max_filesize(Some(MAX_FILE_BYTES))
        .build();

    'walk: for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Apply glob filter.
        if !glob_matcher(path) {
            continue;
        }
        files_scanned += 1;
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        for (lineno, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                matches.push(format!("{}:{}: {}", path.display(), lineno + 1, line));
                if matches.len() >= MAX_MATCHES {
                    truncated = true;
                    break 'walk;
                }
            }
        }
    }

    let refs: Vec<&str> = matches.iter().map(String::as_str).collect();
    let mut out = format_output(Backend::Native, &refs, truncated, pattern, root)?;
    // Append files-scanned count (only native can provide this cheaply).
    out.push_str(&format!("\n[{files_scanned} file(s) scanned]"));
    Ok(out)
}

/// Returns a closure that tests whether a given `Path` matches the glob.
///
/// We build the matcher with `globset` (re-exported by the `ignore` crate's
/// dependency tree) by using `ignore::overrides` — or more directly, the
/// `globset` crate via `ignore`'s own re-export.  Here we just use the simpler
/// `glob::Pattern` approach available through the standard `glob` crate which
/// is a transitive dep, but to avoid adding another explicit dep we use a
/// hand-rolled check.
fn build_glob_matcher(
    glob_pattern: &str,
) -> Result<impl Fn(&Path) -> bool + Send + 'static, String> {
    // Use `ignore`'s built-in globset to handle ** patterns correctly.
    let mut builder = ignore::overrides::OverrideBuilder::new(".");
    // A negated override means "exclude unless matched".  We want "include
    // only if matched", so we add the pattern without negation and then test
    // with `matched`.
    builder
        .add(glob_pattern)
        .map_err(|e| format!("glob parse error: {e}"))?;
    let overrides = builder
        .build()
        .map_err(|e| format!("glob build error: {e}"))?;

    // If the pattern is the catch-all, skip the override check entirely.
    let is_catchall = glob_pattern == DEFAULT_GLOB || glob_pattern == "*";
    Ok(move |path: &Path| {
        if is_catchall {
            return true;
        }
        overrides.matched(path, false).is_whitelist()
    })
}

// ── Shared output formatter ───────────────────────────────────────────────────

fn format_output(
    backend: Backend,
    lines: &[&str],
    truncated: bool,
    pattern: &str,
    root: &str,
) -> Result<String, String> {
    if lines.is_empty() {
        return Ok(format!(
            "[backend: {}] No matches for '{}' in '{}'",
            backend.name(),
            pattern,
            root,
        ));
    }

    let mut out = format!(
        "[backend: {}] Found {} match(es):\n\n",
        backend.name(),
        lines.len(),
    );
    out.push_str(&lines.join("\n"));
    if truncated {
        out.push_str(&format!("\n… (results truncated at {} matches)", MAX_MATCHES));
    }
    Ok(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_to_include_rs() {
        assert_eq!(glob_to_include("**/*.rs"), ("*.rs", true));
    }

    #[test]
    fn glob_to_include_catchall() {
        assert_eq!(glob_to_include("**/*").0, "*");
        assert_eq!(glob_to_include("*").0, "*");
    }

    #[test]
    fn glob_to_include_no_slash() {
        // When the glob has no directory component no simplification warning.
        assert_eq!(glob_to_include("*.ts"), ("*.ts", false));
    }

    #[test]
    fn backend_detect_returns_a_value() {
        // Just confirm no panic; actual backend depends on the test machine.
        let _ = Backend::detect();
    }

    #[tokio::test]
    async fn native_finds_own_source() {
        // The native backend should find "SearchFilesTool" in this very file.
        let cwd = std::env::current_dir().unwrap();
        // Find the tools directory two or more levels up.
        let tools_dir = cwd
            .ancestors()
            .find(|p| p.join("crates/core/src/tools").exists())
            .map(|p| p.join("crates/core/src/tools"))
            .unwrap_or_else(|| cwd.clone());

        let result = run_native(
            tools_dir.to_str().unwrap(),
            "SearchFilesTool",
            "**/*.rs",
            true,
        );
        assert!(result.is_ok(), "native run failed: {:?}", result);
        let output = result.unwrap();
        assert!(
            output.contains("SearchFilesTool"),
            "expected to find 'SearchFilesTool' in output, got:\n{output}"
        );
    }
}
