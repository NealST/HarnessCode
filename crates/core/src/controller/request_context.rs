use serde::{Deserialize, Serialize};

/// A lightweight conversation turn carried into request judgment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
    System,
}

/// A recent conversation message relevant to the current request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: String,
}

/// Structured session state distilled from earlier turns and pipeline outputs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    pub execution_summary: Option<String>,
    pub last_scope: Option<serde_json::Value>,
    pub last_plan: Option<serde_json::Value>,
    pub persistent_summary: Option<String>,
    #[serde(default)]
    pub clarified_facts: Vec<String>,
    #[serde(default)]
    pub known_relevant_files: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

impl SessionState {
    pub fn has_any_state(&self) -> bool {
        self.execution_summary.as_ref().is_some_and(|s| !s.trim().is_empty())
            || self.last_scope.is_some()
            || self.last_plan.is_some()
            || self.persistent_summary.as_ref().is_some_and(|s| !s.trim().is_empty())
            || !self.clarified_facts.is_empty()
            || !self.known_relevant_files.is_empty()
            || !self.open_questions.is_empty()
    }
}

/// Full request context used by routing, scoping, and later planning.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RequestContext {
    pub session_id: Option<String>,
    pub current_request: String,
    pub conversation_summary: Option<String>,
    #[serde(default)]
    pub recent_messages: Vec<ConversationMessage>,
    #[serde(default)]
    pub session_state: SessionState,
}

impl RequestContext {
    pub fn from_prompt(prompt: impl Into<String>) -> Self {
        Self {
            session_id: None,
            current_request: prompt.into(),
            conversation_summary: None,
            recent_messages: Vec::new(),
            session_state: SessionState::default(),
        }
    }

    pub fn has_history(&self) -> bool {
        self.conversation_summary
            .as_ref()
            .is_some_and(|summary| !summary.trim().is_empty())
            || self.recent_messages.iter().any(|message| !message.content.trim().is_empty())
    }

    pub fn has_meaningful_context(&self) -> bool {
        self.has_history() || self.session_state.has_any_state()
    }

    pub fn render_for_agent(&self) -> String {
        let history = if self.recent_messages.is_empty() {
            "(none)".to_string()
        } else {
            self.recent_messages
                .iter()
                .map(|message| {
                    format!(
                        "- {}: {}",
                        match message.role {
                            ConversationRole::User => "user",
                            ConversationRole::Assistant => "assistant",
                            ConversationRole::System => "system",
                        },
                        message.content.trim()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let summary = self
            .conversation_summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or("(none)");
        let execution_summary = self
            .session_state
            .execution_summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or("(none)");
        let persistent_summary = self
            .session_state
            .persistent_summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or("(none)");
        let clarified_facts = if self.session_state.clarified_facts.is_empty() {
            "(none)".to_string()
        } else {
            self.session_state.clarified_facts.join(" | ")
        };
        let known_files = if self.session_state.known_relevant_files.is_empty() {
            "(none)".to_string()
        } else {
            self.session_state.known_relevant_files.join(", ")
        };
        let open_questions = if self.session_state.open_questions.is_empty() {
            "(none)".to_string()
        } else {
            self.session_state.open_questions.join(" | ")
        };

        format!(
            "Current request:\n{current}\n\nConversation summary:\n{summary}\n\nRecent conversation:\n{history}\n\nSession execution state:\n- execution_summary: {execution_summary}\n- persistent_summary: {persistent_summary}\n- clarified_facts: {clarified_facts}\n- known_relevant_files: {known_files}\n- open_questions: {open_questions}\n- last_scope: {last_scope}\n- last_plan: {last_plan}",
            current = self.current_request.trim(),
            summary = summary,
            history = history,
            execution_summary = execution_summary,
            persistent_summary = persistent_summary,
            clarified_facts = clarified_facts,
            known_files = known_files,
            open_questions = open_questions,
            last_scope = self
                .session_state
                .last_scope
                .as_ref()
                .map(serde_json::Value::to_string)
                .unwrap_or_else(|| "null".to_string()),
            last_plan = self
                .session_state
                .last_plan
                .as_ref()
                .map(serde_json::Value::to_string)
                .unwrap_or_else(|| "null".to_string()),
        )
    }

    pub fn apply_clarification(
        &mut self,
        source: &str,
        questions: &[String],
        answer: impl Into<String>,
    ) {
        let answer = answer.into();
        if !questions.is_empty() {
            self.recent_messages.push(ConversationMessage {
                role: ConversationRole::Assistant,
                content: format!(
                    "{source} clarification questions: {}",
                    questions.join(" | ")
                ),
            });
        }
        self.recent_messages.push(ConversationMessage {
            role: ConversationRole::User,
            content: answer.clone(),
        });
        self.session_state.open_questions.clear();
        self.session_state.clarified_facts.push(format!(
            "{source}: {} => {}",
            questions.join(" | "),
            answer.trim()
        ));

        match self.conversation_summary.as_mut() {
            Some(summary) if !summary.trim().is_empty() => {
                summary.push_str("\nUser clarification: ");
                summary.push_str(answer.trim());
            }
            _ => {
                self.conversation_summary = Some(format!("User clarification: {}", answer.trim()));
            }
        }

        match self.session_state.persistent_summary.as_mut() {
            Some(summary) if !summary.trim().is_empty() => {
                summary.push_str("\n");
                summary.push_str(answer.trim());
            }
            _ => {
                self.session_state.persistent_summary = Some(answer.trim().to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_context_from_prompt_is_minimal() {
        let context = RequestContext::from_prompt("Refactor login flow");
        assert_eq!(context.session_id, None);
        assert_eq!(context.current_request, "Refactor login flow");
        assert!(!context.has_meaningful_context());
    }

    #[test]
    fn request_context_detects_history_and_session_state() {
        let context = RequestContext {
            session_id: Some("default".into()),
            current_request: "Continue with the previous plan".into(),
            conversation_summary: Some("User already approved the migration strategy.".into()),
            recent_messages: vec![ConversationMessage {
                role: ConversationRole::User,
                content: "Use the same table split we discussed above".into(),
            }],
            session_state: SessionState {
                execution_summary: Some("Planner produced a three-step migration plan".into()),
                last_scope: None,
                last_plan: None,
                persistent_summary: Some("User approved the migration strategy and table split.".into()),
                clarified_facts: vec!["Schema split is allowed".into()],
                known_relevant_files: vec!["crates/core/src/controller/controller.rs".into()],
                open_questions: Vec::new(),
            },
        };

        assert!(context.has_history());
        assert!(context.has_meaningful_context());
        assert!(context.render_for_agent().contains("previous plan"));
    }
}