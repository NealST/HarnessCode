//! Auto-generation of `AGENTS.md` files following the open [agents.md](https://agents.md/) format.
//!
//! Scans the project directory to detect the tech stack, build/test commands,
//! and project structure, then renders a Markdown document that any
//! agents.md-compatible coding agent can consume.

use std::path::Path;

// ──────────────────────────────────────────────
// Detection helpers
// ──────────────────────────────────────────────

/// Detected project metadata used to populate the AGENTS.md sections.
#[derive(Debug, Default)]
struct ProjectMeta {
    name: String,
    description: String,
    languages: Vec<String>,
    build_commands: Vec<String>,
    test_commands: Vec<String>,
    lint_commands: Vec<String>,
    conventions: Vec<String>,
    structure_summary: String,
}

/// The section headings we consider necessary for a complete AGENTS.md.
const REQUIRED_SECTIONS: &[&str] = &[
    "project overview",
    "project structure",
    "setup & build commands",
    "testing instructions",
];

/// Generate the AGENTS.md content for a project rooted at `root`.
pub fn generate(root: &Path) -> String {
    let meta = detect(root);
    render(&meta)
}

/// Check whether `content` contains all required section headings.
pub fn has_required_sections(content: &str) -> bool {
    let lower = content.to_lowercase();
    REQUIRED_SECTIONS.iter().all(|s| lower.contains(s))
}

/// Load the AGENTS.md at `root`, auto-generating or completing it if needed.
///
/// Returns the final AGENTS.md content that is ready to inject into the LLM prompt.
/// Side-effect: writes/updates the file on disk when generation is required.
pub fn ensure_complete(root: &Path) -> String {
    let agents_path = root.join("AGENTS.md");

    if agents_path.exists() {
        if let Ok(existing) = std::fs::read_to_string(&agents_path) {
            if has_required_sections(&existing) {
                return existing;
            }
            // File exists but incomplete — append missing sections.
            let generated = generate(root);
            let merged = merge_missing_sections(&existing, &generated);
            let _ = std::fs::write(&agents_path, &merged);
            return merged;
        }
    }

    // File doesn't exist — generate from scratch.
    let content = generate(root);
    let _ = std::fs::write(&agents_path, &content);
    content
}

/// Append sections from `generated` that are missing in `existing`.
fn merge_missing_sections(existing: &str, generated: &str) -> String {
    let existing_lower = existing.to_lowercase();
    let mut result = existing.to_string();

    // Split generated into sections by "## " headings.
    for section in generated.split("\n## ").skip(1) {
        // The heading is the first line of the section.
        let heading = section.lines().next().unwrap_or("").trim().to_lowercase();
        if !existing_lower.contains(&heading) {
            result.push_str("\n## ");
            result.push_str(section);
        }
    }

    result
}

// ──────────────────────────────────────────────
// Project detection
// ──────────────────────────────────────────────

fn detect(root: &Path) -> ProjectMeta {
    let mut meta = ProjectMeta::default();

    // ── Name ──────────────────────────────────────────────────────────────
    meta.name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());

    // ── Rust (Cargo.toml) ─────────────────────────────────────────────────
    let cargo_toml = root.join("Cargo.toml");
    if cargo_toml.exists() {
        meta.languages.push("Rust".to_string());
        if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
            if let Some(desc) = extract_toml_string(&content, "description") {
                meta.description = desc;
            }
            if let Some(name) = extract_toml_string(&content, "name") {
                meta.name = name;
            }
        }
        meta.build_commands.push("`cargo build`".to_string());
        meta.test_commands.push("`cargo test`".to_string());
        meta.lint_commands.push("`cargo clippy`".to_string());
        meta.conventions.push("Follow idiomatic Rust (clippy clean, `rustfmt` formatted).".to_string());
    }

    // ── Node / JS / TS (package.json) ─────────────────────────────────────
    let pkg_json = root.join("package.json");
    if pkg_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&pkg_json) {
            detect_node(&content, &mut meta);
        }
    }

    // ── Python (pyproject.toml / requirements.txt / setup.py) ─────────────
    if root.join("pyproject.toml").exists()
        || root.join("requirements.txt").exists()
        || root.join("setup.py").exists()
    {
        meta.languages.push("Python".to_string());
        if root.join("pyproject.toml").exists() {
            meta.build_commands.push("`pip install -e .`".to_string());
        }
        meta.test_commands.push("`pytest`".to_string());
        meta.lint_commands.push("`ruff check .`".to_string());
        meta.conventions.push("Follow PEP 8 style guidelines.".to_string());
    }

    // ── Go (go.mod) ──────────────────────────────────────────────────────
    if root.join("go.mod").exists() {
        meta.languages.push("Go".to_string());
        meta.build_commands.push("`go build ./...`".to_string());
        meta.test_commands.push("`go test ./...`".to_string());
        meta.lint_commands.push("`golangci-lint run`".to_string());
        meta.conventions.push("Follow Go conventions (gofmt, effective Go).".to_string());
    }

    // ── Tauri detection ──────────────────────────────────────────────────
    if has_file_recursive(root, "tauri.conf.json", 3) {
        meta.conventions.push("This project uses Tauri v2 for desktop app packaging.".to_string());
    }

    // ── Monorepo markers ─────────────────────────────────────────────────
    if root.join("pnpm-workspace.yaml").exists() || root.join("lerna.json").exists() {
        meta.conventions.push("This is a monorepo. Check individual package directories for their own build/test instructions.".to_string());
    }

    // ── Structure ────────────────────────────────────────────────────────
    meta.structure_summary = build_structure_summary(root);

    // ── Fallback description ─────────────────────────────────────────────
    if meta.description.is_empty() {
        if let Ok(readme) = std::fs::read_to_string(root.join("README.md")) {
            // Use first non-heading, non-empty line as description.
            for line in readme.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    meta.description = trimmed.to_string();
                    break;
                }
            }
        }
    }

    meta
}

fn detect_node(content: &str, meta: &mut ProjectMeta) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(content) else {
        return;
    };

    // Detect TypeScript vs JavaScript
    let has_ts = json
        .get("devDependencies")
        .and_then(|d| d.get("typescript"))
        .is_some();
    if has_ts {
        if !meta.languages.contains(&"TypeScript".to_string()) {
            meta.languages.push("TypeScript".to_string());
        }
    } else if !meta.languages.contains(&"JavaScript".to_string()) {
        meta.languages.push("JavaScript".to_string());
    }

    // Name / description
    if meta.description.is_empty() {
        if let Some(desc) = json.get("description").and_then(|v| v.as_str()) {
            meta.description = desc.to_string();
        }
    }

    // Package manager
    let pm = if json.get("packageManager").and_then(|v| v.as_str()).unwrap_or("").contains("pnpm") {
        "pnpm"
    } else if json.get("packageManager").and_then(|v| v.as_str()).unwrap_or("").contains("yarn") {
        "yarn"
    } else {
        "npm"
    };

    meta.build_commands.push(format!("`{pm} install`"));

    // Detect scripts
    if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("build") {
            meta.build_commands.push(format!("`{pm} run build`"));
        }
        if scripts.contains_key("dev") {
            meta.build_commands.push(format!("`{pm} run dev` (development server)"));
        }
        if scripts.contains_key("test") {
            meta.test_commands.push(format!("`{pm} test`"));
        }
        if scripts.contains_key("lint") {
            meta.lint_commands.push(format!("`{pm} run lint`"));
        }
    }
}

/// Naïvely extract a bare `key = "value"` string from TOML content.
fn extract_toml_string(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(key) {
            if let Some(after_eq) = trimmed.strip_prefix(key)?.strip_prefix(|c: char| c == ' ' || c == '=') {
                let s = after_eq.trim().trim_start_matches('=').trim();
                if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                    return Some(s[1..s.len() - 1].to_string());
                }
            }
        }
    }
    None
}

/// Check if a file name exists within `max_depth` levels under `root`.
fn has_file_recursive(root: &Path, name: &str, max_depth: usize) -> bool {
    has_file_impl(root, name, max_depth, 0)
}

fn has_file_impl(dir: &Path, name: &str, max_depth: usize, current: usize) -> bool {
    if current > max_depth {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let fname = entry.file_name();
        if fname.to_string_lossy() == name {
            return true;
        }
        if entry.path().is_dir() && !is_noise_dir(&fname.to_string_lossy()) {
            if has_file_impl(&entry.path(), name, max_depth, current + 1) {
                return true;
            }
        }
    }
    false
}

fn is_noise_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "__pycache__" | ".next" | "dist" | ".cache" | ".turbo"
    )
}

// ──────────────────────────────────────────────
// Structure summary
// ──────────────────────────────────────────────

fn build_structure_summary(root: &Path) -> String {
    let mut entries: Vec<String> = Vec::new();
    collect_structure(root, "", 2, 0, &mut entries, 80);

    if entries.is_empty() {
        return "(unable to read project directory)".to_string();
    }

    let root_display = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    format!("{root_display}/\n{}", entries.join("\n"))
}

fn collect_structure(
    dir: &Path,
    prefix: &str,
    max_depth: usize,
    current_depth: usize,
    entries: &mut Vec<String>,
    max_entries: usize,
) {
    if current_depth >= max_depth || entries.len() >= max_entries {
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };

    let mut children: Vec<std::fs::DirEntry> = read_dir.filter_map(|e| e.ok()).collect();

    // Dirs first, then files, alphabetical.
    children.sort_by(|a, b| {
        let a_dir = a.path().is_dir();
        let b_dir = b.path().is_dir();
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.file_name().cmp(&b.file_name()),
        }
    });

    let count = children.len();
    for (idx, child) in children.into_iter().enumerate() {
        if entries.len() >= max_entries {
            break;
        }
        let name = child.file_name().to_string_lossy().to_string();
        // Skip hidden files/dirs at top level (except important ones)
        if name.starts_with('.') && !matches!(name.as_str(), ".github" | ".vscode") {
            continue;
        }
        let is_last = idx + 1 == count;
        let connector = if is_last { "└── " } else { "├── " };
        let child_path = child.path();

        if child_path.is_dir() {
            if is_noise_dir(&name) {
                continue;
            }
            entries.push(format!("{prefix}{connector}{name}/"));
            let new_prefix = format!("{prefix}{}   ", if is_last { " " } else { "│" });
            collect_structure(
                &child_path,
                &new_prefix,
                max_depth,
                current_depth + 1,
                entries,
                max_entries,
            );
        } else {
            entries.push(format!("{prefix}{connector}{name}"));
        }
    }
}

// ──────────────────────────────────────────────
// Render
// ──────────────────────────────────────────────

fn render(meta: &ProjectMeta) -> String {
    let mut md = String::with_capacity(2048);

    // Title
    md.push_str("# AGENTS.md\n\n");

    // Project overview
    md.push_str("## Project overview\n\n");
    md.push_str(&format!("**{}**", meta.name));
    if !meta.description.is_empty() {
        md.push_str(&format!(" — {}", meta.description));
    }
    md.push('\n');
    if !meta.languages.is_empty() {
        md.push_str(&format!(
            "\nLanguages / stack: {}\n",
            meta.languages.join(", ")
        ));
    }
    md.push('\n');

    // Project structure
    if !meta.structure_summary.is_empty() {
        md.push_str("## Project structure\n\n```\n");
        md.push_str(&meta.structure_summary);
        md.push_str("\n```\n\n");
    }

    // Setup / build commands
    if !meta.build_commands.is_empty() {
        md.push_str("## Setup & build commands\n\n");
        for cmd in &meta.build_commands {
            md.push_str(&format!("- {cmd}\n"));
        }
        md.push('\n');
    }

    // Testing
    if !meta.test_commands.is_empty() {
        md.push_str("## Testing instructions\n\n");
        for cmd in &meta.test_commands {
            md.push_str(&format!("- {cmd}\n"));
        }
        md.push_str("- Fix any test failures before submitting your changes.\n");
        md.push('\n');
    }

    // Linting
    if !meta.lint_commands.is_empty() {
        md.push_str("## Lint & formatting\n\n");
        for cmd in &meta.lint_commands {
            md.push_str(&format!("- {cmd}\n"));
        }
        md.push('\n');
    }

    // Code style / conventions
    if !meta.conventions.is_empty() {
        md.push_str("## Code style & conventions\n\n");
        for c in &meta.conventions {
            md.push_str(&format!("- {c}\n"));
        }
        md.push('\n');
    }

    md
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn generate_detects_cargo_project() {
        let tmp = std::env::temp_dir().join("agents_md_test_cargo");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(
            tmp.join("Cargo.toml"),
            r#"[package]
name = "test-project"
description = "A test project"
"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("src/main.rs"), "fn main() {}").unwrap();

        let result = generate(&tmp);

        assert!(result.contains("# AGENTS.md"));
        assert!(result.contains("test-project"));
        assert!(result.contains("Rust"));
        assert!(result.contains("`cargo build`"));
        assert!(result.contains("`cargo test`"));
        assert!(result.contains("src/"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn has_required_sections_detects_complete() {
        let _full = generate(std::env::temp_dir().parent().unwrap_or(Path::new("/")));
        // A generated file for any real directory should at least have overview + structure.
        // We test with a known-good string instead:
        let good = "\
## Project overview\nfoo\n\
## Project structure\nbar\n\
## Setup & build commands\nbaz\n\
## Testing instructions\nqux\n";
        assert!(has_required_sections(good));

        let incomplete = "## Project overview\nfoo\n";
        assert!(!has_required_sections(incomplete));
    }

    #[test]
    fn ensure_complete_creates_file_when_missing() {
        let tmp = std::env::temp_dir().join("agents_md_test_ensure");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"tst\"\n").unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("src/lib.rs"), "").unwrap();

        let result = ensure_complete(&tmp);
        assert!(has_required_sections(&result));
        assert!(tmp.join("AGENTS.md").exists());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_complete_keeps_existing_when_complete() {
        let tmp = std::env::temp_dir().join("agents_md_test_keep");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let custom = "\
# My Custom AGENTS.md\n\n\
## Project overview\nMy awesome project\n\n\
## Project structure\nsrc/\n\n\
## Setup & build commands\nmake build\n\n\
## Testing instructions\nmake test\n";
        fs::write(tmp.join("AGENTS.md"), custom).unwrap();

        let result = ensure_complete(&tmp);
        // Should return the original content untouched.
        assert_eq!(result, custom);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_complete_merges_missing_sections() {
        let tmp = std::env::temp_dir().join("agents_md_test_merge");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"tst\"\n").unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("src/lib.rs"), "").unwrap();

        // Write a partial file with only overview.
        let partial = "# AGENTS.md\n\n## Project overview\nMy project\n";
        fs::write(tmp.join("AGENTS.md"), partial).unwrap();

        let result = ensure_complete(&tmp);
        // Should now contain the missing sections.
        assert!(has_required_sections(&result));
        // Should preserve the original overview text.
        assert!(result.contains("My project"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
