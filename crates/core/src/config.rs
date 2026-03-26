//! # HarnessCode Configuration
//!
//! Implements a layered configuration system. Settings are resolved in this
//! priority order (highest → lowest):
//!
//! 1. **Environment variables** — `HARNESSCODE_PROFILE`, `OPENAI_API_KEY`, etc.
//! 2. **Project-level config** — `.harness.toml` in the current working directory
//! 3. **User-level config** — `~/.harness/config.toml`
//! 4. **Built-in defaults** — `openai` provider, `gpt-4o` model
//!
//! ## Config file format
//!
//! ```toml
//! default_profile = "openai"
//!
//! [profiles.openai]
//! provider = "openai-compatible"
//! model = "gpt-4o"
//! base_url = "https://api.openai.com/v1"
//! api_key = "sk-..."
//!
//! [profiles.anthropic]
//! provider = "anthropic"
//! model = "claude-3-5-sonnet-20241022"
//! api_key = "sk-ant-..."
//!
//! [profiles.ollama]
//! provider = "openai-compatible"
//! model = "llama3.2"
//! base_url = "http://localhost:11434/v1"
//! # no api_key needed for local Ollama
//! ```

use crate::llm::{
    anthropic::AnthropicProvider, openai_compatible::OpenAICompatibleProvider, LlmError,
    LlmProvider,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tracing::debug;

// ──────────────────────────────────────────────
// Config file structures
// ──────────────────────────────────────────────

/// A single named provider profile (one entry under `[profiles.<name>]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// `"openai-compatible"` or `"anthropic"`.
    pub provider: String,
    /// Model identifier, e.g. `"gpt-4o"` or `"claude-3-5-sonnet-20241022"`.
    pub model: String,
    /// Base URL for the API. Required for `openai-compatible`; ignored for `anthropic`.
    pub base_url: Option<String>,
    /// API key. If absent, the env var `<PROFILE_NAME_UPPER>_API_KEY` is tried,
    /// then the conventional default (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`).
    pub api_key: Option<String>,
}

/// Top-level structure of a `config.toml` / `.harness.toml` file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HarnessConfig {
    /// Which profile to use when none is specified explicitly.
    pub default_profile: Option<String>,
    /// Maximum tool-call turns before the guardrail terminates the agent loop.
    /// Defaults to 100 when absent.
    pub max_tool_turns: Option<usize>,
    /// Named provider profiles.
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
}

impl HarnessConfig {
    /// Parse a TOML string into a [`HarnessConfig`].
    pub fn from_toml(content: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(content)
    }

    /// Serialize this config back to a TOML string.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

// ──────────────────────────────────────────────
// Config resolution
// ──────────────────────────────────────────────

/// The well-known file name for project-level config (hidden file, added to .gitignore).
pub const PROJECT_CONFIG_FILE: &str = ".harness.toml";

/// The well-known path for user-level config relative to the home directory.
pub const USER_CONFIG_RELATIVE: &str = ".harness/config.toml";

/// Return the path to the user-level config file (`~/.harness/config.toml`).
pub fn user_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(USER_CONFIG_RELATIVE))
}

/// Return the path to the project-level config file (`.harness.toml` in cwd).
pub fn project_config_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(PROJECT_CONFIG_FILE)
}

/// Load and merge the layered config.
///
/// User-level config is loaded first, then project-level is merged on top
/// (project values take precedence).
pub fn load_config() -> HarnessConfig {
    let mut config = HarnessConfig::default();

    // Layer 1: user-level (~/.harness/config.toml)
    if let Some(path) = user_config_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            debug!(path = %path.display(), "Loading user-level config");
            if let Ok(user_cfg) = HarnessConfig::from_toml(&content) {
                merge_config(&mut config, user_cfg);
            }
        }
    }

    // Layer 2: project-level (.harness.toml in cwd)
    let project_path = project_config_path();
    if let Ok(content) = std::fs::read_to_string(&project_path) {
        debug!(path = %project_path.display(), "Loading project-level config");
        if let Ok(project_cfg) = HarnessConfig::from_toml(&content) {
            merge_config(&mut config, project_cfg);
        }
    }

    config
}

/// Merge `src` on top of `base` (src values win for matching keys).
fn merge_config(base: &mut HarnessConfig, src: HarnessConfig) {
    if src.default_profile.is_some() {
        base.default_profile = src.default_profile;
    }
    if src.max_tool_turns.is_some() {
        base.max_tool_turns = src.max_tool_turns;
    }
    for (name, profile) in src.profiles {
        base.profiles.insert(name, profile);
    }
}

// ──────────────────────────────────────────────
// Provider construction
// ──────────────────────────────────────────────

/// Construct the active [`LlmProvider`] using the full layered resolution:
///
/// 1. `HARNESSCODE_PROFILE` env var (overrides the profile name)
/// 2. `default_profile` from config files
/// 3. Falls back to built-in defaults (`openai` / `gpt-4o`)
///
/// API keys are resolved per profile in this order:
/// 1. `api_key` field in the profile config
/// 2. Environment variable named `<PROFILE_NAME_UPPER>_API_KEY`
///    (e.g. profile `deepseek` → `DEEPSEEK_API_KEY`)
/// 3. Conventional default: `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`
pub fn default_provider() -> Result<Arc<dyn LlmProvider>, LlmError> {
    let config = load_config();
    build_provider_from_config(&config, None)
}

/// Build a provider for a specific named profile (or the default if `None`).
pub fn provider_for_profile(
    profile_name: Option<&str>,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let config = load_config();
    build_provider_from_config(&config, profile_name)
}

fn build_provider_from_config(
    config: &HarnessConfig,
    profile_override: Option<&str>,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    // Determine the active profile name
    let profile_name = profile_override
        .map(|s| s.to_string())
        .or_else(|| std::env::var("HARNESSCODE_PROFILE").ok())
        .or_else(|| config.default_profile.clone())
        .unwrap_or_else(|| "openai".to_string());

    if let Some(profile) = config.profiles.get(&profile_name) {
        let api_key = resolve_api_key(profile, &profile_name);
        build_provider_from_profile(profile, api_key)
    } else {
        // No profile in config — fall back to environment variables
        build_provider_from_env(&profile_name)
    }
}

/// Resolve the API key for a profile using the priority chain.
fn resolve_api_key(profile: &ProfileConfig, profile_name: &str) -> Option<String> {
    // 1. Explicit api_key in config
    if profile.api_key.is_some() {
        return profile.api_key.clone();
    }

    // 2. Profile-named env var: <PROFILE_NAME_UPPER>_API_KEY
    let profile_env = format!("{}_API_KEY", profile_name.to_uppercase());
    if let Ok(key) = std::env::var(&profile_env) {
        return Some(key);
    }

    // 3. Conventional fallback based on provider type
    let conventional = match profile.provider.as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    };
    std::env::var(conventional).ok()
}

fn build_provider_from_profile(
    profile: &ProfileConfig,
    api_key: Option<String>,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    match profile.provider.as_str() {
        "anthropic" => {
            let key = api_key.ok_or(LlmError::MissingApiKey {
                provider: "anthropic",
                env_var: "ANTHROPIC_API_KEY",
            })?;
            Ok(Arc::new(AnthropicProvider::with_config(key, &profile.model)))
        }
        _ => {
            // openai-compatible (OpenAI, Azure, Ollama, DeepSeek, Groq, …)
            let base_url = profile
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            // Local providers (e.g. Ollama) don't need a key
            let key = api_key.unwrap_or_default();
            Ok(Arc::new(OpenAICompatibleProvider::with_config(
                key, &profile.model, base_url,
            )))
        }
    }
}

/// Pure env-var fallback when no matching profile exists in config.
fn build_provider_from_env(profile_name: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
    match profile_name {
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| LlmError::MissingApiKey {
                provider: "anthropic",
                env_var: "ANTHROPIC_API_KEY",
            })?;
            let model = std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| "claude-3-5-sonnet-20241022".to_string());
            Ok(Arc::new(AnthropicProvider::with_config(key, model)))
        }
        _ => {
            let key = std::env::var("OPENAI_API_KEY").map_err(|_| LlmError::MissingApiKey {
                provider: "openai",
                env_var: "OPENAI_API_KEY",
            })?;
            let model =
                std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
            let base_url = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
            Ok(Arc::new(OpenAICompatibleProvider::with_config(
                key, model, base_url,
            )))
        }
    }
}

// ──────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_toml() {
        let toml = r#"
            default_profile = "openai"

            [profiles.openai]
            provider = "openai-compatible"
            model = "gpt-4o"
            base_url = "https://api.openai.com/v1"
            api_key = "sk-test"

            [profiles.anthropic]
            provider = "anthropic"
            model = "claude-3-5-sonnet-20241022"
            api_key = "sk-ant-test"

            [profiles.ollama]
            provider = "openai-compatible"
            model = "llama3.2"
            base_url = "http://localhost:11434/v1"
        "#;

        let cfg = HarnessConfig::from_toml(toml).unwrap();
        assert_eq!(cfg.default_profile.unwrap(), "openai");
        assert_eq!(cfg.profiles.len(), 3);

        let openai = cfg.profiles.get("openai").unwrap();
        assert_eq!(openai.model, "gpt-4o");
        assert_eq!(openai.api_key.as_deref(), Some("sk-test"));

        let ollama = cfg.profiles.get("ollama").unwrap();
        assert!(ollama.api_key.is_none());
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn merge_config_project_wins() {
        let mut base = HarnessConfig {
            default_profile: Some("openai".to_string()),
            max_tool_turns: None,
            profiles: {
                let mut m = HashMap::new();
                m.insert(
                    "openai".to_string(),
                    ProfileConfig {
                        provider: "openai-compatible".to_string(),
                        model: "gpt-4o".to_string(),
                        base_url: None,
                        api_key: Some("user-key".to_string()),
                    },
                );
                m
            },
        };

        let project = HarnessConfig {
            default_profile: Some("anthropic".to_string()),
            max_tool_turns: None,
            profiles: {
                let mut m = HashMap::new();
                m.insert(
                    "openai".to_string(),
                    ProfileConfig {
                        provider: "openai-compatible".to_string(),
                        model: "gpt-4o-mini".to_string(),
                        base_url: None,
                        api_key: Some("project-key".to_string()),
                    },
                );
                m
            },
        };

        merge_config(&mut base, project);

        // Project default_profile wins
        assert_eq!(base.default_profile.unwrap(), "anthropic");
        // Project profile values win
        assert_eq!(base.profiles["openai"].model, "gpt-4o-mini");
        assert_eq!(base.profiles["openai"].api_key.as_deref(), Some("project-key"));
    }

    #[test]
    fn api_key_resolution_order() {
        let profile = ProfileConfig {
            provider: "openai-compatible".to_string(),
            model: "gpt-4o".to_string(),
            base_url: None,
            api_key: Some("config-key".to_string()),
        };
        // Config file key is highest priority
        assert_eq!(resolve_api_key(&profile, "openai").as_deref(), Some("config-key"));
    }
}
