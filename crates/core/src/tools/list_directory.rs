//! Sensor tool: list the contents of a directory.
//!
//! Returns a tree-style listing, skipping common noise directories
//! (`.git`, `node_modules`, `target`, `__pycache__`, `.next`, `dist`).

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use async_trait::async_trait;
use std::path::Path;

pub struct ListDirectoryTool;

/// Directories to skip unconditionally.
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

/// Cap the total number of entries returned to protect context window.
const MAX_ENTRIES: usize = 500;

#[async_trait]
impl Tool for ListDirectoryTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_directory".into(),
            description: "List the files and directories inside a path. \
                          Skips .git, node_modules, target, and other build/cache directories. \
                          Set depth=1 for a shallow listing (default), or depth=0 for full recursion."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Maximum recursion depth. 0 = unlimited, 1 = top-level only (default).",
                        "default": 1
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let path = match args["path"].as_str() {
            Some(p) => p.to_string(),
            None => return ToolResult::err("Missing required argument: path"),
        };
        let max_depth = args["depth"].as_u64().unwrap_or(1) as usize;
        let effective_depth = if max_depth == 0 { usize::MAX } else { max_depth };

        // Offload synchronous directory I/O to a blocking thread pool so we
        // don't stall the tokio executor.
        let path_clone = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut entries: Vec<String> = Vec::new();
            collect_entries(Path::new(&path_clone), "", effective_depth, 0, &mut entries)
                .map(|_| entries)
        })
        .await;

        let entries = match result {
            Err(e) => return ToolResult::err(format!("Worker thread panic: {}", e)),
            Ok(Err(e)) => return ToolResult::err(format!("Error listing '{}': {}", path, e)),
            Ok(Ok(e)) => e,
        };

        if entries.is_empty() {
            return ToolResult::ok(format!("(empty directory: {})", path));
        }

        let mut output = format!("{}/\n", path);
        let display = if entries.len() > MAX_ENTRIES {
            output += &entries[..MAX_ENTRIES].join("\n");
            output += &format!("\n… (truncated at {} entries)", MAX_ENTRIES);
            output
        } else {
            output += &entries.join("\n");
            output
        };
        ToolResult::ok(display)
    }
}

fn collect_entries(
    dir: &Path,
    prefix: &str,
    max_depth: usize,
    current_depth: usize,
    entries: &mut Vec<String>,
) -> std::io::Result<()> {
    // Stop expanding when we've reached the depth limit.
    // depth=1 means listing top-level children only (current_depth starts at 0).
    if current_depth >= max_depth {
        return Ok(());
    }

    let mut children: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .collect();

    // Deterministic ordering: dirs first, then files, both alphabetically.
    children.sort_by(|a, b| {
        let a_is_dir = a.path().is_dir();
        let b_is_dir = b.path().is_dir();
        match (a_is_dir, b_is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.file_name().cmp(&b.file_name()),
        }
    });

    let count = children.len();
    for (idx, child) in children.into_iter().enumerate() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let name = child.file_name().to_string_lossy().to_string();
        let is_last = idx + 1 == count;
        let connector = if is_last { "└── " } else { "├── " };
        let child_path = child.path();

        if child_path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                entries.push(format!("{}{}{}/  (skipped)", prefix, connector, name));
                continue;
            }
            entries.push(format!("{}{}{}/", prefix, connector, name));
            let new_prefix = format!("{}{}   ", prefix, if is_last { " " } else { "│" });
            collect_entries(&child_path, &new_prefix, max_depth, current_depth + 1, entries)?;
        } else {
            entries.push(format!("{}{}{}", prefix, connector, name));
        }
    }
    Ok(())
}
