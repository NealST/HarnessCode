//! Actuator tool: apply a unified diff to an existing file.
//!
//! Applies hunks from a standard `--- a/…` / `+++ b/…` unified diff.
//! Lines starting with `-` are removed, lines starting with `+` are added,
//! and context lines (no prefix) must match exactly.

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use async_trait::async_trait;

pub struct ApplyDiffTool;

#[async_trait]
impl Tool for ApplyDiffTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "apply_diff".into(),
            description: "Apply a unified diff to a file. \
                          Uses the `path` argument to locate the file — the `---`/`+++` \
                          header lines in the diff are optional and ignored. \
                          The `@@ -N,M +N,M @@` hunk header and context/removal/addition \
                          lines are required. Context lines must match the file exactly."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to patch."
                    },
                    "diff": {
                        "type": "string",
                        "description": "Unified diff string (standard git diff format)."
                    }
                },
                "required": ["path", "diff"]
            }),
        }
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let path = match args["path"].as_str() {
            Some(p) => p.to_string(),
            None => return ToolResult::err("Missing required argument: path"),
        };
        let diff = match args["diff"].as_str() {
            Some(d) => d.to_string(),
            None => return ToolResult::err("Missing required argument: diff"),
        };

        let original = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("Cannot read '{}': {}", path, e)),
        };

        match apply_unified_diff(&original, &diff) {
            Ok(patched) => match tokio::fs::write(&path, &patched).await {
                Ok(()) => ToolResult::ok(format!("✅ Patch applied to '{}'", path)),
                Err(e) => ToolResult::err(format!("Failed to write patched file '{}': {}", path, e)),
            },
            Err(e) => ToolResult::err(format!("Failed to apply diff to '{}': {}", path, e)),
        }
    }
}

/// Minimal unified-diff applicator.
///
/// Handles the common case produced by LLMs: one or more `@@ … @@` hunks
/// each containing context, removed (`-`), and added (`+`) lines.
fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
    let orig_lines: Vec<&str> = original.lines().collect();
    let diff_lines: Vec<&str> = diff.lines().collect();

    // Collect hunks: each hunk is (old_start, Vec<diff_line>)
    let mut hunks: Vec<(usize, Vec<&str>)> = Vec::new();
    let mut i = 0;

    // Skip file header lines (--- / +++)
    while i < diff_lines.len() && (diff_lines[i].starts_with("---") || diff_lines[i].starts_with("+++")) {
        i += 1;
    }

    while i < diff_lines.len() {
        let line = diff_lines[i];
        if line.starts_with("@@") {
            // Parse @@ -old_start[,old_count] +new_start[,new_count] @@
            let old_start = parse_hunk_header(line)?;
            i += 1;
            let mut hunk_lines = Vec::new();
            while i < diff_lines.len() && !diff_lines[i].starts_with("@@") {
                hunk_lines.push(diff_lines[i]);
                i += 1;
            }
            hunks.push((old_start, hunk_lines));
        } else {
            i += 1;
        }
    }

    if hunks.is_empty() {
        return Err("No hunks found in diff".into());
    }

    // Apply hunks in reverse order so line numbers stay valid.
    let mut result: Vec<String> = orig_lines.iter().map(|l| l.to_string()).collect();
    for (old_start, hunk) in hunks.into_iter().rev() {
        result = apply_hunk(result, old_start, &hunk)?;
    }

    Ok(result.join("\n") + if original.ends_with('\n') { "\n" } else { "" })
}

fn parse_hunk_header(line: &str) -> Result<usize, String> {
    // @@ -old_start,old_count +new_start,new_count @@
    let inner = line
        .trim_start_matches('@')
        .trim()
        .split(' ')
        .find(|s| s.starts_with('-'))
        .ok_or_else(|| format!("Malformed hunk header: {line}"))?;
    let num_str = inner.trim_start_matches('-').split(',').next().unwrap_or("1");
    num_str.parse::<usize>()
        .map(|n| if n == 0 { 1 } else { n })
        .map_err(|_| format!("Cannot parse hunk start in: {line}"))
}

fn apply_hunk(lines: Vec<String>, old_start: usize, hunk: &[&str]) -> Result<Vec<String>, String> {
    // old_start is 1-based; convert to 0-based index.
    let pos = old_start.saturating_sub(1);
    let mut result_prefix = lines[..pos].to_vec();
    let mut patch_output: Vec<String> = Vec::new();

    let orig_remaining = &lines[pos..];
    let mut orig_idx = 0;

    for hunk_line in hunk {
        if hunk_line.starts_with('+') {
            patch_output.push(hunk_line[1..].to_string());
        } else if hunk_line.starts_with('-') {
            // Verify context matches before consuming.
            let expected = &hunk_line[1..];
            if orig_idx >= orig_remaining.len() || orig_remaining[orig_idx] != expected {
                return Err(format!(
                    "Hunk mismatch at line {}: expected {:?}, got {:?}",
                    old_start + orig_idx,
                    expected,
                    orig_remaining.get(orig_idx).map(|s| s.as_str()).unwrap_or("<EOF>")
                ));
            }
            orig_idx += 1; // consume — do not emit
        } else {
            // Context line: must match.
            let expected = if hunk_line.starts_with(' ') { &hunk_line[1..] } else { hunk_line };
            if orig_idx < orig_remaining.len() && orig_remaining[orig_idx] == expected {
                patch_output.push(orig_remaining[orig_idx].to_string());
                orig_idx += 1;
            } else if orig_idx >= orig_remaining.len() && expected.is_empty() {
                // Trailing empty context line — tolerate.
            } else {
                return Err(format!(
                    "Context mismatch at line {}: expected {:?}, got {:?}",
                    old_start + orig_idx,
                    expected,
                    orig_remaining.get(orig_idx).map(|s| s.as_str()).unwrap_or("<EOF>")
                ));
            }
        }
    }

    // Append remaining lines after the hunk.
    result_prefix.extend(patch_output);
    result_prefix.extend(orig_remaining[orig_idx..].iter().cloned());
    Ok(result_prefix)
}
