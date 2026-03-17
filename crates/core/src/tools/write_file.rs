//! Actuator tool: write (create or overwrite) a file.
//! Creates parent directories automatically.

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use async_trait::async_trait;
use std::path::Path;

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Write content to a file, creating it (and any parent directories) \
                          if it does not exist, or overwriting it if it does. \
                          Use apply_diff for surgical edits to existing files."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to the file to write."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full text content to write."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let path = match args["path"].as_str() {
            Some(p) => p.to_string(),
            None => return ToolResult::err("Missing required argument: path"),
        };
        let content = match args["content"].as_str() {
            Some(c) => c.to_string(),
            None => return ToolResult::err("Missing required argument: content"),
        };

        // Create parent directories if they don't exist.
        if let Some(parent) = Path::new(&path).parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::err(format!("Failed to create directories for '{}': {}", path, e));
            }
        }

        match tokio::fs::write(&path, &content).await {
            Ok(()) => ToolResult::ok(format!("✅ Written {} bytes to '{}'", content.len(), path)),
            Err(e) => ToolResult::err(format!("Failed to write '{}': {}", path, e)),
        }
    }
}
