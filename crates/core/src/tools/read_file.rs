//! Sensor tool: read a file's contents from the filesystem.

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use async_trait::async_trait;

const MAX_BYTES: usize = 100_000; // ~100 KB — prevents flooding the context window

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read the full contents of a file. Returns the text content. \
                          Truncates at 100 KB to protect the context window."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to the file to read."
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

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                if content.len() > MAX_BYTES {
                    // Find the last char boundary at or before MAX_BYTES so we
                    // never split a multi-byte UTF-8 sequence.
                    let end = (0..=MAX_BYTES)
                        .rev()
                        .find(|&i| content.is_char_boundary(i))
                        .unwrap_or(0);
                    let truncated = &content[..end];
                    ToolResult::ok(format!(
                        "{truncated}\n\n[...truncated at {end} bytes — file has {} total bytes]",
                        content.len()
                    ))
                } else {
                    ToolResult::ok(content)
                }
            }
            Err(e) => ToolResult::err(format!("Failed to read '{}': {}", path, e)),
        }
    }
}
