//! Sensor tool: search file contents for a literal text pattern.
//!
//! Walks the workspace tree, skips noise directories, and returns
//! matching lines with file path and 1-based line numbers.
//!
//! Note: pattern matching is a literal substring search (case-sensitive by
//! default), NOT a regex.

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use async_trait::async_trait;
use std::path::Path;

pub struct SearchFilesTool;

/// Directories to skip.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".next",
    "dist",
    ".cache",
    ".turbo",
];

/// Maximum number of matches returned.
const MAX_MATCHES: usize = 200;

/// Maximum file size to scan (2 MB).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[async_trait]
impl Tool for SearchFilesTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "search_files".into(),
            description: "Search for a text pattern across files in a directory tree. \
                          Returns matching lines with file paths and line numbers. \
                          Pattern is treated as a literal string (case-sensitive by default). \
                          Optionally filter by file extension."
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
                        "description": "Text pattern to search for (literal string)."
                    },
                    "extension": {
                        "type": "string",
                        "description": "Optional file extension filter, e.g. 'rs' or 'ts' (without the dot)."
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
        let ext_filter = args["extension"].as_str().map(|s| s.to_string());
        let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(true);

        let needle = if case_sensitive {
            pattern.clone()
        } else {
            pattern.to_lowercase()
        };

        // Offload blocking filesystem I/O to the thread pool.
        // Clone root/pattern so they remain available for the result message below.
        let root_clone = root.clone();
        let (matches, total_files_scanned, truncated) =
            tokio::task::spawn_blocking(move || {
                let mut matches: Vec<String> = Vec::new();
                let mut total = 0usize;
                let truncated = search_recursive(
                    Path::new(&root_clone),
                    &needle,
                    &ext_filter,
                    case_sensitive,
                    &mut matches,
                    &mut total,
                );
                (matches, total, truncated)
            })
            .await
            .unwrap_or_else(|_| (vec!["Worker thread panic".into()], 0, false));

        if matches.is_empty() {
            return ToolResult::ok(format!(
                "No matches for '{}' in '{}' ({} files scanned)",
                pattern, root, total_files_scanned
            ));
        }

        let mut output = format!(
            "Found {} match(es) in {} file(s) scanned:\n\n",
            matches.len(),
            total_files_scanned
        );
        output += &matches.join("\n");
        if truncated {
            output += &format!("\n… (results truncated at {} matches)", MAX_MATCHES);
        }
        ToolResult::ok(output)
    }
}

/// Returns `true` if results were truncated.
fn search_recursive(
    dir: &Path,
    needle: &str,
    ext_filter: &Option<String>,
    case_sensitive: bool,
    matches: &mut Vec<String>,
    total_files: &mut usize,
) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP_DIRS.contains(&dir_name) {
                continue;
            }
            if search_recursive(&path, needle, ext_filter, case_sensitive, matches, total_files) {
                return true;
            }
        } else if path.is_file() {
            // Extension filter
            if let Some(ext) = ext_filter {
                let file_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if file_ext != ext.as_str() {
                    continue;
                }
            }
            // Skip large files
            if let Ok(meta) = entry.metadata() {
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
            }
            *total_files += 1;
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let path_str = path.display().to_string();
            for (lineno, line) in contents.lines().enumerate() {
                let haystack = if case_sensitive {
                    line.to_string()
                } else {
                    line.to_lowercase()
                };
                if haystack.contains(needle) {
                    matches.push(format!("{}:{}: {}", path_str, lineno + 1, line));
                    if matches.len() >= MAX_MATCHES {
                        return true;
                    }
                }
            }
        }
    }
    false
}
