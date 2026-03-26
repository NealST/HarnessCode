//! Tool: invoke a loaded skill by name and return its rendered body.
//!
//! When the model decides the user's request matches a skill, it calls this
//! tool with `{ "name": "skill-name", "arguments": "..." }`.  The tool
//! renders the skill body (substituting `$ARGUMENTS` etc.) and returns the
//! result as the tool output.  The model then treats that content as additional
//! instructions embedded in the conversation.

use super::{Tool, ToolResult};
use crate::llm::ToolDef;
use crate::skills::SkillRegistry;
use async_trait::async_trait;
use std::sync::Arc;

/// A tool that lets the model invoke a registered skill by name.
pub struct InvokeSkillTool {
    registry: Arc<SkillRegistry>,
}

impl InvokeSkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for InvokeSkillTool {
    fn def(&self) -> ToolDef {
        let skills_list = self.registry.model_tool_description();
        ToolDef {
            name: "invoke_skill".into(),
            description: format!(
                "Invoke a skill by name to load specialised instructions for a task. \
                 Use this when the user's request matches a skill's description. \
                 The skill body is returned as instructions you should follow.\n\n\
                 Available skills:\n{skills_list}"
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill name to invoke (exactly as listed above)."
                    },
                    "arguments": {
                        "type": "string",
                        "description": "Optional arguments to pass to the skill (replaces $ARGUMENTS in the skill body)."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolResult::err("invoke_skill: missing required field 'name'"),
        };
        let skill_args = args
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match self.registry.get(name) {
            Some(skill) => {
                let rendered = skill.render(skill_args);
                ToolResult::ok(rendered)
            }
            None => ToolResult::err(format!(
                "Skill '{name}' not found. Available skills: {}",
                self.registry
                    .list_user_invocable()
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}
