//! # HarnessCode CLI
//!
//! The terminal entry point for the HarnessCode AI coding agent.
//!
//! ## Usage
//!
//! ```text
//! harnesscode [OPTIONS]                   # interactive REPL session
//! harnesscode config init                 # interactive config wizard
//! harnesscode config show                 # print resolved config
//! ```
//!
//! Inside the REPL, lines beginning with `/` are treated as built-in commands:
//!
//! ```text
//! /help                   show this help
//! /init                   generate or update AGENTS.md for this project
//! /session list           list all sessions
//! /session use [id]       switch to a session (interactive picker if no id)
//! /session delete <id>    delete a session
//! /rename [name]          rename the current session
//! /clear                  clear the current session's conversation history
//! /cost                   show conversation cost estimate for this session
//! /exit  /quit            exit the REPL
//! ```

use clap::{Parser, Subcommand};
use harnesscode_core::{
    agents::{AgentOutput, AgentRole},
    commands::{help_text, parse_builtin, BuiltinAgentKind, BuiltinCommand},
    config::{
        load_config, project_config_path, user_config_path,
        HarnessConfig, ProfileConfig, PROJECT_CONFIG_FILE,
    },
    controller::{
        ClarificationCallback, ClarificationRequest, ClarificationResolution, Controller,
        PipelineEvent, RequestContext,
        ScoperSkipCallback, ScoperSkipDecision,
    },
    memory::{FileSessionStore, SessionMemoryPatch, SessionStore},
    skills::SkillRegistry,
};
use indicatif::{ProgressBar, ProgressStyle};
use inquire::{Confirm, Select, Text};
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tracing::{error, info};
use std::sync::Arc;

// ──────────────────────────────────────────────
// CLI argument structure
// ──────────────────────────────────────────────

/// HarnessCode — Safe AI Coding Agent
#[derive(Parser, Debug)]
#[command(
    name = "harnesscode",
    about = "HarnessCode — Cybernetics-inspired AI coding agent",
    version
)]
struct Cli {
    /// Log level: trace | debug | info | warn | error
    #[arg(long, default_value = "info", env = "HARNESSCODE_LOG")]
    log_level: String,

    /// Override the active config profile (e.g. --profile ollama)
    #[arg(long, short = 'p', env = "HARNESSCODE_PROFILE")]
    profile: Option<String>,

    /// Maximum tool-call turns before the agent loop is terminated (default: 100)
    #[arg(long, env = "HARNESSCODE_MAX_TOOL_TURNS")]
    max_tool_turns: Option<usize>,

    /// Session id used for shared cross-run memory.
    #[arg(long, env = "HARNESSCODE_SESSION", default_value = "default")]
    session: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage HarnessCode configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Interactive wizard to create or update a config file
    Init,
    /// Print the currently resolved configuration (merged from all layers)
    Show,
}

// ──────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────

fn print_banner() {
    println!(
        r#"
 ██╗  ██╗ █████╗ ██████╗ ███╗   ██╗███████╗███████╗███████╗     ██████╗ ██████╗ ██████╗ ███████╗
 ██║  ██║██╔══██╗██╔══██╗████╗  ██║██╔════╝██╔════╝██╔════╝    ██╔════╝██╔═══██╗██╔══██╗██╔════╝
 ███████║███████║██████╔╝██╔██╗ ██║█████╗  ███████╗███████╗    ██║     ██║   ██║██║  ██║█████╗  
 ██╔══██║██╔══██║██╔══██╗██║╚██╗██║██╔══╝  ╚════██║╚════██║    ██║     ██║   ██║██║  ██║██╔══╝  
 ██║  ██║██║  ██║██║  ██║██║ ╚████║███████╗███████║███████║    ╚██████╗╚██████╔╝██████╔╝███████╗
 ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝╚══════╝╚══════╝╚══════╝     ╚═════╝ ╚═════╝ ╚═════╝ ╚══════╝
"#
    );
    println!("  Welcome to HarnessCode - Safe AI Coding Agent");
    println!("  Cybernetics · Multi-Agent · Absolute Safety\n");
}

fn make_spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

// ──────────────────────────────────────────────
// config init
// ──────────────────────────────────────────────

fn cmd_config_init() {
    println!("\n🔧  HarnessCode Config Wizard\n");

    // ── Determine target location ────────────────────────────────────────────
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = dirs::home_dir().unwrap_or_default();
    let is_home = cwd == home;

    // Default scope: project if cwd ≠ home, else user
    let default_scope = if is_home { "User  (~/.harness/config.toml)" } else { "Project (.harness.toml)" };
    let other_scope   = if is_home { "Project (.harness.toml)" } else { "User  (~/.harness/config.toml)" };

    let scope = Select::new(
        "Where should the config be saved?",
        vec![default_scope, other_scope],
    )
    .prompt()
    .unwrap_or(default_scope);

    let write_project = scope.starts_with("Project");

    // ── Profile name ─────────────────────────────────────────────────────────
    let profile_name = Text::new("Profile name:")
        .with_default("openai")
        .with_help_message("e.g. openai | anthropic | ollama | deepseek")
        .prompt()
        .unwrap_or_else(|_| "openai".to_string());

    // ── Provider type ─────────────────────────────────────────────────────────
    let provider = Select::new(
        "Provider type:",
        vec!["openai-compatible", "anthropic"],
    )
    .prompt()
    .unwrap_or("openai-compatible");

    // ── Model ────────────────────────────────────────────────────────────────
    let default_model = match provider {
        "anthropic" => "claude-3-5-sonnet-20241022",
        _ => "gpt-4o",
    };
    let model = Text::new("Model name:")
        .with_default(default_model)
        .prompt()
        .unwrap_or_else(|_| default_model.to_string());

    // ── Base URL (OpenAI-compatible only) ────────────────────────────────────
    let base_url = if provider == "openai-compatible" {
        let default_url = "https://api.openai.com/v1";
        Some(
            Text::new("Base URL:")
                .with_default(default_url)
                .with_help_message("Change for Azure, Ollama (http://localhost:11434/v1), DeepSeek, etc.")
                .prompt()
                .unwrap_or_else(|_| default_url.to_string()),
        )
    } else {
        None
    };

    // ── API Key ───────────────────────────────────────────────────────────────
    let api_key_input = Text::new("API Key (leave blank to skip — you can set it later):")
        .with_help_message("Stored in the config file. Keep this file private.")
        .prompt()
        .unwrap_or_default();

    let api_key = if api_key_input.trim().is_empty() {
        None
    } else {
        Some(api_key_input.trim().to_string())
    };

    // ── Build config ──────────────────────────────────────────────────────────
    let mut profiles = HashMap::new();
    profiles.insert(
        profile_name.clone(),
        ProfileConfig {
            provider: provider.to_string(),
            model,
            base_url,
            api_key,
        },
    );

    let config = HarnessConfig {
        default_profile: Some(profile_name.clone()),
        max_tool_turns: None,
        profiles,
    };

    // ── Write file ────────────────────────────────────────────────────────────
    if write_project {
        let path = project_config_path();
        std::fs::write(&path, config.to_toml())
            .expect("Failed to write project config file");
        println!("\n✅  Config written to {}", path.display());

        // Auto-add to .gitignore
        ensure_gitignore_entry(PROJECT_CONFIG_FILE);

        // Offer to also set as user-level default
        let also_user = Confirm::new("Also save as user-level default (~/.harness/config.toml)?")
            .with_default(false)
            .prompt()
            .unwrap_or(false);

        if also_user {
            write_user_config(&config);
        }
    } else {
        write_user_config(&config);
    }

    println!("\n💡  Run `harnesscode config show` to verify the resolved configuration.");
    println!("💡  Use `harnesscode --profile {}` to use this profile explicitly.\n", profile_name);
}

fn write_user_config(config: &HarnessConfig) {
    if let Some(path) = user_config_path() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("Failed to create ~/.harness/");
        }
        std::fs::write(&path, config.to_toml())
            .expect("Failed to write user config file");
        println!("✅  Config written to {}", path.display());
    }
}

/// Ensure `.harness.toml` appears in the project's `.gitignore`.
fn ensure_gitignore_entry(entry: &str) {
    let gitignore_path = std::env::current_dir()
        .unwrap_or_default()
        .join(".gitignore");

    // Read existing content (or start empty)
    let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();

    if existing.lines().any(|l| l.trim() == entry) {
        // Already present — nothing to do
        return;
    }

    let newline = '\n';
    let new_content = if existing.ends_with(newline) || existing.is_empty() {
        format!("{}{}{newline}", existing, entry)
    } else {
        format!("{}{newline}{}{newline}", existing, entry)
    };

    match std::fs::write(&gitignore_path, new_content) {
        Ok(_) => println!("✅  Added `{}` to .gitignore", entry),
        Err(e) => println!("⚠️  Could not update .gitignore: {e}"),
    }
}

// ──────────────────────────────────────────────
// config show
// ──────────────────────────────────────────────

fn cmd_config_show() {
    let config = load_config();
    println!("\n📋  Resolved HarnessCode Configuration\n");

    let default = config
        .default_profile
        .as_deref()
        .unwrap_or("<none — will use 'openai'>");
    println!("  default_profile  = \"{default}\"");

    let max_turns = config.max_tool_turns.unwrap_or(100);
    println!("  max_tool_turns   = {max_turns}\n");

    if config.profiles.is_empty() {
        println!("  (no profiles configured)\n");
    } else {
        let mut names: Vec<_> = config.profiles.keys().collect();
        names.sort();
        for name in names {
            let p = &config.profiles[name];
            let active = if config.default_profile.as_deref() == Some(name) { " ◀ active" } else { "" };
            println!("  [profiles.{name}]{active}");
            println!("    provider = \"{}\"", p.provider);
            println!("    model    = \"{}\"", p.model);
            if let Some(url) = &p.base_url {
                println!("    base_url = \"{url}\"");
            }
            if p.api_key.is_some() {
                println!("    api_key  = \"***\" (set)");
            } else {
                println!("    api_key  = (not set)");
            }
            println!();
        }
    }

    // Show which files were loaded
    if let Some(user_path) = user_config_path() {
        let marker = if user_path.exists() { "✅" } else { "  " };
        println!("  {marker} User config:    {}", user_path.display());
    }
    let proj_path = project_config_path();
    let marker = if proj_path.exists() { "✅" } else { "  " };
    println!("  {marker} Project config: {}", proj_path.display());
    println!();
}

// ──────────────────────────────────────────────
// Formatted result display
// ──────────────────────────────────────────────

/// Print a colourised summary of all pipeline outputs.
fn print_pipeline_result(outputs: &[AgentOutput]) {
    println!("🎉  Pipeline completed successfully!\n");

    for output in outputs {
        let icon = if output.role == AgentRole::Risk {
            if output.summary.starts_with("[HIGH]") { "🚨" }
            else if output.summary.starts_with("[MEDIUM]") { "⚠️" }
            else { "✅" }
        } else if output.role == AgentRole::Judge {
            "⚖️"
        } else if output.role == AgentRole::Scoper {
            "🧭"
        } else if output.success { "✅" } else { "❌" };
        println!("  {icon}  [{:<8}]  {}", output.role.to_string(), output.summary);

        if output.role == AgentRole::Judge {
            if let Some(effective_request) = output.payload.get("effective_request").and_then(|v| v.as_str()) {
                println!("        Effective request: {effective_request}");
            }
            if let Some(questions) = output.payload.get("clarifying_questions").and_then(|v| v.as_array()) {
                for question in questions.iter().filter_map(|v| v.as_str()) {
                    println!("        ? {question}");
                }
            }
        }

        if output.role == AgentRole::Scoper {
            if let Some(objective) = output.payload.get("objective").and_then(|v| v.as_str()) {
                println!("        Objective: {objective}");
            }
            if let Some(criteria) = output.payload.get("success_criteria").and_then(|v| v.as_array()) {
                for criterion in criteria.iter().filter_map(|v| v.as_str()) {
                    println!("        • {criterion}");
                }
            }
            if let Some(questions) = output.payload.get("clarifying_questions").and_then(|v| v.as_array()) {
                for question in questions.iter().filter_map(|v| v.as_str()) {
                    println!("        ? {question}");
                }
            }
        }

        // ── Planner: show numbered steps if present ──────────────────────────
        if output.role == AgentRole::Planner {
            if let Some(steps) = output.payload.get("steps").and_then(|s| s.as_array()) {
                for (i, step) in steps.iter().enumerate() {
                    let text = step.as_str().unwrap_or_default();
                    println!("        {}. {}", i + 1, text);
                }
            }
        }

        // ── Conductor: colourised diff ────────────────────────────────────────
        if output.role == AgentRole::Conductor {
            if let Some(diff) = output.payload.get("diff").and_then(|d| d.as_str()) {
                println!();
                for line in diff.lines() {
                    if line.starts_with('+') && !line.starts_with("+++") {
                        // Green for additions
                        println!("        \x1b[32m{line}\x1b[0m");
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        // Red for deletions
                        println!("        \x1b[31m{line}\x1b[0m");
                    } else if line.starts_with("@@") {
                        // Cyan for hunk headers
                        println!("        \x1b[36m{line}\x1b[0m");
                    } else {
                        println!("        {line}");
                    }
                }
                println!();
            }
        }

        // ── Risk: show semantic assessment ──────────────────────────────────
        if output.role == AgentRole::Risk {
            let risk_level = output.payload
                .get("risk_level").and_then(|v| v.as_str()).unwrap_or("low");
            let (risk_colour, risk_icon) = match risk_level {
                "high"   => ("\x1b[31m", "🚨"),
                "medium" => ("\x1b[33m", "⚠️ "),
                _        => ("\x1b[32m", "✅"),
            };
            println!("        {risk_icon}  Level: {risk_colour}{}\x1b[0m", risk_level.to_uppercase());
            if let Some(areas) = output.payload.get("affected_areas").and_then(|a| a.as_array()) {
                if !areas.is_empty() {
                    let list: Vec<_> = areas.iter().filter_map(|a| a.as_str()).collect();
                    println!("        Affected areas: {}", list.join(" · "));
                }
            }
            if output.payload.get("breaking_change").and_then(|b| b.as_bool()).unwrap_or(false) {
                println!("        \x1b[31m⚡ Breaking change detected\x1b[0m");
            }
            if let Some(sec) = output.payload.get("security_implications").and_then(|s| s.as_str()) {
                if !sec.is_empty() {
                    println!("        🔒 Security: {sec}");
                }
            }
            if let Some(focus) = output.payload.get("cr_focus").and_then(|f| f.as_str()) {
                if !focus.is_empty() {
                    println!("        👁  CR Focus: {focus}");
                }
            }
        }

        // ── Reviewer: structured advisory output ─────────────────────────────
        if output.role == AgentRole::Reviewer {
            let approved = output.payload.get("approved").and_then(|v| v.as_bool()).unwrap_or(false);
            let criteria_met = output.payload.get("criteria_met").and_then(|v| v.as_bool()).unwrap_or(false);

            let (verdict_icon, verdict_colour) = if approved && criteria_met {
                ("✅", "\x1b[32m")
            } else {
                ("⚠️ ", "\x1b[33m")
            };
            let criteria_icon = if criteria_met { "\x1b[32m✅" } else { "\x1b[31m❌" };

            println!("        {verdict_colour}{verdict_icon}  {}\x1b[0m", output.summary);
            println!("        {criteria_icon}  Success criteria {}\x1b[0m", if criteria_met { "met" } else { "NOT met" });

            if let Some(issues) = output.payload.get("issues").and_then(|i| i.as_array()) {
                if !issues.is_empty() {
                    println!("        \x1b[33mIssues:\x1b[0m");
                    for issue in issues {
                        println!("          • {}", issue.as_str().unwrap_or_default());
                    }
                }
            }

            if let Some(concerns) = output.payload.get("security_concerns").and_then(|c| c.as_array()) {
                if !concerns.is_empty() {
                    println!("        \x1b[31m🔒 Security concerns:\x1b[0m");
                    for concern in concerns {
                        println!("          • {}", concern.as_str().unwrap_or_default());
                    }
                }
            }
        }

        println!();
    }
}

// ──────────────────────────────────────────────
// current_session file helpers
// ──────────────────────────────────────────────

/// Path to the file that persists the "current session" across REPL invocations.
fn current_session_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".harness")
        .join("current_session")
}

/// Load the persisted current session id, or return `"default"`.
fn load_current_session() -> String {
    std::fs::read_to_string(current_session_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Write `session_id` to the current_session marker file.
fn save_current_session(session_id: &str) {
    let path = current_session_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, session_id);
}

// ──────────────────────────────────────────────
// Main
// ──────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialise tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.parse().unwrap_or_default()),
        )
        .with_target(false)
        .compact()
        .init();

    // ── Subcommands ──────────────────────────────────────────────────────────
    if let Some(cmd) = cli.command {
        match cmd {
            Commands::Config { action } => {
                match action {
                    ConfigAction::Init => cmd_config_init(),
                    ConfigAction::Show => cmd_config_show(),
                }
                return;
            }
        }
    }

    // ── Banner ───────────────────────────────────────────────────────────────
    print_banner();

    // ── LLM setup ────────────────────────────────────────────────────────────
    let llm = match harnesscode_core::config::provider_for_profile(cli.profile.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("❌  LLM configuration error: {e}");
            eprintln!("    Run `harnesscode config init` to set up your API credentials.");
            return;
        }
    };

    // ── Session store ─────────────────────────────────────────────────────────
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::for_project(cwd.clone()));

    // ── Skill registry ────────────────────────────────────────────────────────
    let skill_registry = SkillRegistry::load(&cwd);

    // ── Resolve starting session ──────────────────────────────────────────────
    // Priority: --session flag > .harness/current_session file > "default"
    let mut current_session_id = if cli.session != "default" {
        cli.session.clone()
    } else {
        load_current_session()
    };
    save_current_session(&current_session_id);

    // ── Build controller ──────────────────────────────────────────────────────
    let controller = {
        let mut c = Controller::new(3, Arc::clone(&llm)).with_memory(Arc::clone(&store));
        let max_turns = cli.max_tool_turns.or_else(|| load_config().max_tool_turns);
        if let Some(turns) = max_turns {
            c = c.with_max_tool_turns(turns);
        }
        c
    };
    let controller = Arc::new(controller);

    // ── Show current session info ─────────────────────────────────────────────
    if let Ok(Some(mem)) = store.get_session(&current_session_id).await {
        let title = mem.title.as_deref().unwrap_or(&current_session_id);
        let turns = mem.conversation_turns.len();
        println!("  \x1b[2m📎  Session: \x1b[0m\x1b[36m{}\x1b[0m\x1b[2m  ({})\x1b[0m", current_session_id, title);
        if turns > 0 {
            println!("  \x1b[2m💬  {} turn{} in history\x1b[0m", turns, if turns == 1 { "" } else { "s" });
        }
    } else {
        println!("  \x1b[2m📎  Session: \x1b[36m{}\x1b[0m\x1b[2m  (new)\x1b[0m", current_session_id);
    }
    println!("  \x1b[2mType /help for built-in commands, or start typing your request.\x1b[0m\n");

    // ── REPL loop ─────────────────────────────────────────────────────────────
    loop {
        // Prompt — show session id in dim text
        let prompt_label = format!("\x1b[2m[{}]\x1b[0m 💬 ", current_session_id);
        let input = match Text::new(&prompt_label)
            .with_placeholder("Ask me anything, or type / for commands…")
            .prompt()
        {
            Ok(t) => t,
            Err(_) => {
                // Ctrl+C / Ctrl+D / EOF — exit gracefully
                println!("\n  Goodbye! 👋");
                break;
            }
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        // ── Handle built-in /commands ─────────────────────────────────────────
        if let Some(cmd) = parse_builtin(trimmed) {
            match cmd {
                BuiltinCommand::Help => {
                    print!("{}", help_text());
                }

                BuiltinCommand::Exit => {
                    println!("  Goodbye! 👋");
                    break;
                }

                BuiltinCommand::Clear => {
                    match store.clear_session(&current_session_id).await {
                        Ok(_) => println!("  ✅  Session '{}' history cleared.", current_session_id),
                        Err(e) => eprintln!("  ❌  Failed to clear session: {e}"),
                    }
                }

                BuiltinCommand::Init => {
                    let dest = cwd.join("AGENTS.md");
                    if dest.exists() {
                        let overwrite = Confirm::new("AGENTS.md already exists. Overwrite?")
                            .with_default(false)
                            .prompt()
                            .unwrap_or(false);
                        if !overwrite {
                            println!("  Aborted.");
                            continue;
                        }
                    }
                    let content = harnesscode_core::commands::generate_agents_md(&cwd);
                    match std::fs::write(&dest, &content) {
                        Ok(()) => println!("  ✅  Generated {}", dest.display()),
                        Err(e) => eprintln!("  ❌  Failed to write AGENTS.md: {e}"),
                    }
                }

                BuiltinCommand::Cost => {
                    match store.get_session(&current_session_id).await {
                        Ok(Some(mem)) => {
                            let turns = mem.conversation_turns.len();
                            // Rough estimate: each turn ~ 700 chars, ~175 tokens
                            let est_tokens: usize = mem.conversation_turns
                                .iter()
                                .map(|t| (t.request.len() + t.response_summary.len()) / 4)
                                .sum();
                            println!(
                                "\n  \x1b[1m/cost\x1b[0m  session: \x1b[36m{}\x1b[0m",
                                current_session_id
                            );
                            println!("  Turns in history : {turns}");
                            println!("  Est. history tokens: ~{est_tokens}");
                            if let Some(ref summary) = mem.compacted_summary {
                                let compacted_tokens = summary.len() / 4;
                                println!("  Compacted summary  : ~{compacted_tokens} tokens");
                            }
                            println!();
                        }
                        Ok(None) => println!("  No session data yet."),
                        Err(e) => eprintln!("  ❌  {e}"),
                    }
                }

                BuiltinCommand::Rename(name) => {
                    let new_title = match name {
                        Some(n) => n,
                        None => match Text::new("New session name:").prompt() {
                            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
                            _ => { println!("  Aborted."); continue; }
                        },
                    };
                    let patch = SessionMemoryPatch {
                        title: Some(new_title.clone()),
                        ..SessionMemoryPatch::default()
                    };
                    match store.patch_session(&current_session_id, patch).await {
                        Ok(_) => println!("  ✅  Session renamed to \"{}\"", new_title),
                        Err(e) => eprintln!("  ❌  {e}"),
                    }
                }

                BuiltinCommand::SessionList => {
                    match store.list_sessions().await {
                        Ok(sessions) if sessions.is_empty() => {
                            println!("  (no saved sessions)");
                        }
                        Ok(sessions) => {
                            println!("\n  \x1b[1mSessions\x1b[0m\n");
                            println!("  {:<3}  {:<24}  {:<30}  {}", "", "SESSION ID", "TITLE", "LAST UPDATED");
                            println!("  {}", "─".repeat(80));
                            for s in &sessions {
                                let active = if s.session_id == current_session_id { "▶" } else { " " };
                                let title = s.title.as_deref().unwrap_or("—");
                                let dt = chrono_or_secs(s.updated_at_secs);
                                println!(
                                    "  \x1b[36m{active}\x1b[0m  {:<24}  {:<30}  {}",
                                    truncate_str(&s.session_id, 24),
                                    truncate_str(title, 30),
                                    dt,
                                );
                            }
                            println!();
                        }
                        Err(e) => eprintln!("  ❌  {e}"),
                    }
                }

                BuiltinCommand::SessionUse(id) => {
                    let target_id = match id {
                        Some(id) => id,
                        None => {
                            // Interactive picker
                            match store.list_sessions().await {
                                Ok(sessions) if !sessions.is_empty() => {
                                    let options: Vec<String> = sessions
                                        .iter()
                                        .map(|s| {
                                            let title = s.title.as_deref().unwrap_or("—");
                                            let dt = chrono_or_secs(s.updated_at_secs);
                                            let marker = if s.session_id == current_session_id { " ◀ current" } else { "" };
                                            format!("{} — {} [{}]{}", s.session_id, title, dt, marker)
                                        })
                                        .collect();
                                    // Also offer creating a new session
                                    let mut all_options = options;
                                    all_options.push("+ New session".to_string());
                                    match Select::new("Select a session:", all_options.clone()).prompt() {
                                        Ok(choice) if choice == "+" || choice.starts_with("+ New") => {
                                            match Text::new("New session id:").prompt() {
                                                Ok(n) if !n.trim().is_empty() => n.trim().to_string(),
                                                _ => { println!("  Aborted."); continue; }
                                            }
                                        }
                                        Ok(choice) => {
                                            // Extract the session_id (before " — ")
                                            choice.splitn(2, " — ").next().unwrap_or("default").trim().to_string()
                                        }
                                        Err(_) => { println!("  Aborted."); continue; }
                                    }
                                }
                                Ok(_) => {
                                    // No sessions yet — ask for a name
                                    match Text::new("New session id:").prompt() {
                                        Ok(n) if !n.trim().is_empty() => n.trim().to_string(),
                                        _ => { println!("  Aborted."); continue; }
                                    }
                                }
                                Err(e) => { eprintln!("  ❌  {e}"); continue; }
                            }
                        }
                    };

                    if target_id == current_session_id {
                        println!("  Already on session \x1b[36m{}\x1b[0m.", current_session_id);
                        continue;
                    }

                    current_session_id = target_id.clone();
                    save_current_session(&current_session_id);

                    // Show brief info about the newly active session
                    match store.get_session(&current_session_id).await {
                        Ok(Some(mem)) => {
                            let title = mem.title.as_deref().unwrap_or(&current_session_id);
                            let turns = mem.conversation_turns.len();
                            println!(
                                "  ✅  Switched to session \x1b[36m{}\x1b[0m  \"{}\"  ({} turn{})",
                                current_session_id, title, turns,
                                if turns == 1 { "" } else { "s" }
                            );
                        }
                        Ok(None) => {
                            println!("  ✅  Switched to new session \x1b[36m{}\x1b[0m", current_session_id);
                        }
                        Err(e) => eprintln!("  ⚠️  Switched session but could not load info: {e}"),
                    }
                }

                BuiltinCommand::SessionDelete(id) => {
                    let confirm = Confirm::new(&format!("Delete session '{id}'? This cannot be undone."))
                        .with_default(false)
                        .prompt()
                        .unwrap_or(false);
                    if !confirm {
                        println!("  Aborted.");
                        continue;
                    }
                    match store.delete_session(&id).await {
                        Ok(()) => {
                            println!("  ✅  Session '{}' deleted.", id);
                            // If we just deleted the active session, fall back to "default"
                            if id == current_session_id {
                                current_session_id = "default".to_string();
                                save_current_session(&current_session_id);
                                println!("  📎  Switched to session 'default'.");
                            }
                        }
                        Err(e) => eprintln!("  ❌  {e}"),
                    }
                }

                BuiltinCommand::RunAgent { agent, args } => {
                    let agent_label = match agent {
                        BuiltinAgentKind::Scoper    => "Scoper",
                        BuiltinAgentKind::Compactor => "Compactor",
                    };
                    if matches!(agent, BuiltinAgentKind::Scoper) && args.is_empty() {
                        eprintln!("  ⚠️  Usage: /scope <task description>");
                    } else {
                        info!(agent = %agent_label, "Running single agent via CLI");
                        println!();

                        let (tx, mut rx) = tokio::sync::mpsc::channel::<PipelineEvent>(16);
                        let session_id = current_session_id.clone();
                        let controller_clone = Arc::clone(&controller);
                        let args_clone = args.clone();

                        let handle = tokio::spawn(async move {
                            controller_clone
                                .run_single_agent(agent, &args_clone, Some(&session_id), Some(tx))
                                .await
                        });

                        let mut current_pb: Option<ProgressBar> = None;
                        while let Some(event) = rx.recv().await {
                            match event {
                                PipelineEvent::StageStarted { role } => {
                                    if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                    let (icon, label) = stage_label(role);
                                    let pb = make_spinner(&format!("{icon}  {label}   正在处理…"));
                                    current_pb = Some(pb);
                                }
                                PipelineEvent::StageCompleted { output } => {
                                    if let Some(pb) = current_pb.take() {
                                        let (icon, label) = stage_label(output.role);
                                        pb.finish_with_message(format!(
                                            "✅  {icon}  {label}   {}",
                                            output.summary
                                        ));
                                    }
                                    // Role-specific detail — no full-pipeline banner.
                                    if output.role == AgentRole::Scoper {
                                        println!();
                                        if let Some(obj) = output.payload.get("objective").and_then(|v| v.as_str()) {
                                            println!("        \x1b[1mObjective:\x1b[0m {obj}");
                                        }
                                        if let Some(criteria) = output.payload.get("success_criteria").and_then(|v| v.as_array()) {
                                            for c in criteria.iter().filter_map(|v| v.as_str()) {
                                                println!("        • {c}");
                                            }
                                        }
                                        if let Some(qs) = output.payload.get("clarifying_questions").and_then(|v| v.as_array()) {
                                            for q in qs.iter().filter_map(|v| v.as_str()) {
                                                println!("        \x1b[33m? {q}\x1b[0m");
                                            }
                                        }
                                        println!();
                                    }
                                }
                                PipelineEvent::PipelineFailed { error } => {
                                    if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                    eprintln!("  ❌  {error}");
                                }
                                _ => {}
                            }
                        }
                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                        if let Err(e) = handle.await {
                            eprintln!("  ❌  Agent error: {e}");
                        }
                    }
                }

                BuiltinCommand::InvokeSkill { name, args } => {
                    match skill_registry.get(&name) {
                        Some(skill) => {
                            // Render the skill body and send it as a task to the AI pipeline.
                            let task = skill.render(&args);
                            info!(skill = %name, "Invoking skill via CLI");
                            println!();

                            let (tx, mut rx) = tokio::sync::mpsc::channel::<PipelineEvent>(16);
                            let session_id = current_session_id.clone();
                            let controller_clone = Arc::clone(&controller);

                            let pipeline = tokio::spawn(async move {
                                let mut request_context = RequestContext::from_prompt(task);
                                request_context.session_id = Some(session_id);
                                controller_clone
                                    .run_with_request_context(
                                        &request_context,
                                        Some(tx),
                                        None,
                                        None,
                                        None,
                                    )
                                    .await
                            });

                            let mut current_pb: Option<ProgressBar> = None;
                            while let Some(event) = rx.recv().await {
                                match event {
                                    PipelineEvent::StageStarted { role } => {
                                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                        let (icon, label) = stage_label(role);
                                        let pb = make_spinner(&format!("{icon}  {label}   正在处理…"));
                                        current_pb = Some(pb);
                                    }
                                    PipelineEvent::StageCompleted { output } => {
                                        if let Some(pb) = current_pb.take() {
                                            let (icon, label) = stage_label(output.role);
                                            pb.finish_with_message(format!("✅  {icon}  {label}   {}", output.summary));
                                        }
                                    }
                                    PipelineEvent::StageSkipped { role } => {
                                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                        let (icon, label) = stage_label(role);
                                        println!("  ⏭️   {icon}  {label}   skipped");
                                    }
                                    PipelineEvent::StageRetrying { role, reason, attempt } => {
                                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                        let (icon, label) = stage_label(role);
                                        eprintln!("  ⚠️   {icon}  {label}   retry (attempt {attempt}): {reason}");
                                        let pb = make_spinner(&format!("{icon}  {label}   正在重试…"));
                                        current_pb = Some(pb);
                                    }
                                    PipelineEvent::RiskAssessed {
                                        risk_level, reason, affected_areas,
                                        breaking_change, security_implications, cr_focus, risk_unavailable,
                                    } => {
                                        if risk_unavailable {
                                            println!("  ⚠️   Risk assessment unavailable\n");
                                        } else {
                                            let (colour, icon) = match risk_level.as_str() {
                                                "high"   => ("\x1b[31m", "🚨"),
                                                "medium" => ("\x1b[33m", "⚠️ "),
                                                _        => ("\x1b[32m", "✅"),
                                            };
                                            println!("        {icon}  Level: {colour}{}  \x1b[0m{reason}", risk_level.to_uppercase());
                                            if !affected_areas.is_empty() {
                                                println!("        Affected: {}", affected_areas.join(" · "));
                                            }
                                            if breaking_change { println!("        \x1b[31m⚡ Breaking change\x1b[0m"); }
                                            if !security_implications.is_empty() { println!("        🔒 Security: {security_implications}"); }
                                            if !cr_focus.is_empty() { println!("        👁  CR Focus: {cr_focus}"); }
                                            println!();
                                        }
                                    }
                                    PipelineEvent::ReviewCompleted {
                                        approved, criteria_met, issues, security_concerns, recommendation,
                                    } => {
                                        let (icon, colour) = if approved {
                                            ("✅", "\x1b[32m")
                                        } else {
                                            ("⚠️ ", "\x1b[33m")
                                        };
                                        println!("        {icon}  Review: {colour}{recommendation}\x1b[0m");
                                        let criteria_icon = if criteria_met { "\x1b[32m✅" } else { "\x1b[31m❌" };
                                        println!("        {criteria_icon}  Success criteria {}\x1b[0m",
                                            if criteria_met { "met" } else { "NOT met" });
                                        if !issues.is_empty() {
                                            println!("        \x1b[33mIssues:\x1b[0m");
                                            for issue in &issues { println!("          • {issue}"); }
                                        }
                                        if !security_concerns.is_empty() {
                                            println!("        \x1b[31m🔒 Security concerns:\x1b[0m");
                                            for concern in &security_concerns { println!("          • {concern}"); }
                                        }
                                        println!();
                                    }
                                    PipelineEvent::PipelineFailed { error } => {
                                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                        eprintln!("  ❌  {error}");
                                    }
                                    PipelineEvent::PipelineRetrying { reason, attempt } => {
                                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                        eprintln!("  🔄  Pipeline restarting (attempt {attempt}): {reason}");
                                    }
                                    PipelineEvent::StageAborted { role, reason } => {
                                        if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                                        let (icon, label) = stage_label(role);
                                        eprintln!("  ⚠️   {icon}  {label}   aborted: {reason}");
                                    }
                                    _ => {}
                                }
                            }
                            if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }

                            if let Err(e) = pipeline.await {
                                eprintln!("  ❌  Pipeline error: {e}");
                            }
                        }
                        None => {
                            eprintln!("  ⚠️  Unknown skill or command: /{name}");
                            eprintln!("  💡  Type /help for available commands.");
                        }
                    }
                }

                BuiltinCommand::Unknown(msg) => {
                    eprintln!("  ⚠️  {msg}");
                }
            }
            continue;
        }

        // ── AI pipeline ───────────────────────────────────────────────────────
        let task = trimmed.to_string();
        info!(task = %task, session = %current_session_id, "Task received");
        println!();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<PipelineEvent>(16);
        let task_clone = task.clone();
        let session_id = current_session_id.clone();
        let controller_clone = Arc::clone(&controller);

        let clarification_callback: ClarificationCallback = Arc::new(move |request: ClarificationRequest| {
            Box::pin(async move {
                let prompt = format!(
                    "{} needs clarification:\n{}\n\nAnswer:",
                    request.source,
                    request.questions.join("\n- ")
                );
                match tokio::task::spawn_blocking(move || {
                    Text::new(&prompt)
                        .with_placeholder("Provide the missing detail, or leave blank to abort")
                        .prompt()
                })
                .await
                {
                    Ok(Ok(answer)) if !answer.trim().is_empty() => ClarificationResolution::Answer(answer),
                    _ => ClarificationResolution::Abort,
                }
            })
        });

        let scoper_skip_callback: ScoperSkipCallback = Arc::new(move |effective_request: String| {
            Box::pin(async move {
                let preview: String = effective_request.chars().take(80).collect();
                let label = format!(
                    "🧭  Scoper will frame: '{}'{} — skip and go straight to planning?",
                    preview,
                    if effective_request.len() > 80 { "…" } else { "" },
                );
                match tokio::task::spawn_blocking(move || {
                    Confirm::new(&label)
                        .with_default(false)
                        .with_help_message("Yes = skip Scoper, No = let Scoper frame the problem first")
                        .prompt()
                })
                .await
                {
                    Ok(Ok(true)) => ScoperSkipDecision::Skip,
                    _ => ScoperSkipDecision::Run,
                }
            })
        });

        let pipeline = tokio::spawn(async move {
            let mut request_context = RequestContext::from_prompt(task_clone);
            request_context.session_id = Some(session_id);
            controller_clone
                .run_with_request_context(
                    &request_context,
                    Some(tx),
                    None,
                    Some(clarification_callback),
                    Some(scoper_skip_callback),
                )
                .await
        });

        fn stage_label(role: AgentRole) -> (&'static str, &'static str) {
            match role {
                AgentRole::Judge     => ("⚖️", "Judge   "),
                AgentRole::Scoper    => ("🧭", "Scoper  "),
                AgentRole::Planner   => ("🧠", "Planner "),
                AgentRole::Conductor => ("💻", "Conductor"),
                AgentRole::Risk      => ("🛡️", "Risk    "),
                AgentRole::Reviewer  => ("🔍", "Reviewer"),
                AgentRole::Compactor => ("🗜️", "Compactor"),
            }
        }

        let mut current_pb: Option<ProgressBar> = None;

        while let Some(event) = rx.recv().await {
            match event {
                PipelineEvent::StageStarted { role } => {
                    if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                    let (icon, label) = stage_label(role);
                    let pb = make_spinner(&format!("{icon}  {label}   正在处理…"));
                    current_pb = Some(pb);
                }
                PipelineEvent::StageSkipped { role } => {
                    if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                    let (icon, label) = stage_label(role);
                    println!("  ⏭  {icon}  {label}   \x1b[2m(skipped)\x1b[0m");
                }
                PipelineEvent::StageRetrying { role, reason, attempt } => {
                    // Keep the spinner running but update its message so the user
                    // knows a retry is in progress and why the first attempt failed.
                    let (icon, label) = stage_label(role);
                    if let Some(ref pb) = current_pb {
                        pb.set_message(format!(
                            "{icon}  {label}   \x1b[33m⟳ 重试中 (attempt {attempt} failed: {reason})…\x1b[0m"
                        ));
                    }
                }
                PipelineEvent::StageCompleted { output } => {
                    if let Some(pb) = current_pb.take() {
                        let (icon, label) = stage_label(output.role);
                        if output.role == AgentRole::Risk {
                            let (prefix, colour) = if output.summary.starts_with("[HIGH]") {
                                ("🚨", "\x1b[31m")
                            } else if output.summary.starts_with("[MEDIUM]") {
                                ("⚠️ ", "\x1b[33m")
                            } else {
                                ("✅", "\x1b[32m")
                            };
                            pb.finish_with_message(format!(
                                "{prefix}  {icon}  {label}   {colour}{}\x1b[0m",
                                output.summary
                            ));
                        } else {
                            pb.finish_with_message(format!(
                                "✅  {icon}  {label}   {}",
                                output.summary
                            ));
                        }
                    }
                }
                PipelineEvent::JudgeReady {
                    route,
                    route_reason_code,
                    effective_request,
                    goal_is_concrete,
                    constraints_are_stable,
                    history_resolves_references,
                    repository_grounding_needed,
                    prior_scope_can_be_reused,
                    skip_scoper_criteria_met,
                    ready_for_scoper,
                    ready_for_planner,
                    ask_user_clarification,
                    clarifying_questions,
                    ..
                } => {
                    println!("\n  ⚖️  Judge Decision\n");
                    println!("    Effective request: {effective_request}");
                    println!("    Route: {route} ({route_reason_code})");
                    println!(
                        "    Flags: planner={} scoper={} clarify={}",
                        ready_for_planner, ready_for_scoper, ask_user_clarification,
                    );
                    println!(
                        "    Criteria: goal_concrete={} constraints_stable={} history_resolves_refs={} repo_grounding_needed={} prior_scope_reusable={}",
                        goal_is_concrete, constraints_are_stable, history_resolves_references,
                        repository_grounding_needed, prior_scope_can_be_reused,
                    );
                    for criterion in &skip_scoper_criteria_met { println!("    • skip scoper: {criterion}"); }
                    for question in &clarifying_questions { println!("    ? {question}"); }
                    println!();
                }
                PipelineEvent::ScopeReady {
                    task_type, objective, unknowns, success_criteria,
                    relevant_files, needs_user_clarification, clarifying_questions, ..
                } => {
                    println!("\n  🧭  Problem Frame  ({})\n", task_type.to_uppercase());
                    println!("    Objective: {objective}");
                    for criterion in &success_criteria { println!("    • {criterion}"); }
                    if !unknowns.is_empty() {
                        println!("\n  ❓  Unknowns:");
                        for item in &unknowns { println!("      • {item}"); }
                    }
                    if needs_user_clarification && !clarifying_questions.is_empty() {
                        println!("\n  🗣️  Clarifying questions:");
                        for question in &clarifying_questions { println!("      ? {question}"); }
                    }
                    if !relevant_files.is_empty() {
                        println!("\n  📁  Relevant files:");
                        for file in &relevant_files { println!("      • {file}"); }
                    }
                    println!();
                }
                PipelineEvent::ClarificationRequested { source, objective, questions } => {
                    println!("\n  ❓  Clarification Requested by {source}\n");
                    println!("    Objective: {objective}");
                    for question in &questions { println!("    ? {question}"); }
                    println!();
                }
                PipelineEvent::PlanReady { phase_count, phases, complexity } => {
                    let complexity_colour = match complexity.as_str() {
                        "high"   => "\x1b[31m",
                        "medium" => "\x1b[33m",
                        _        => "\x1b[32m",
                    };
                    println!(
                        "\n  📋  Execution Plan  ({phase_count} phase(s), complexity: {complexity_colour}{}\x1b[0m)\n",
                        complexity.to_uppercase()
                    );
                    for p in &phases {
                        println!(
                            "    Phase {}: {}  [{} step(s), {}]",
                            p.phase_id, p.title, p.step_count, p.complexity
                        );
                    }
                    println!();
                }
                PipelineEvent::PhaseStarted { phase_id, title, total_phases } => {
                    if let Some(pb) = current_pb.take() { pb.finish_and_clear(); }
                    let pb = make_spinner(&format!(
                        "⚙️   Phase {phase_id}/{total_phases}: {title}   正在执行…"
                    ));
                    current_pb = Some(pb);
                }
                PipelineEvent::PhaseCompleted { phase_id, title, total_phases, explanation, files_changed, affected_files } => {
                    if let Some(pb) = current_pb.take() {
                        pb.finish_with_message(format!(
                            "✅  Phase {phase_id}/{total_phases}: {title}   {explanation}  \x1b[2m({files_changed} file(s) changed)\x1b[0m"
                        ));
                    }
                    if !affected_files.is_empty() {
                        for f in &affected_files {
                            println!("   \x1b[2m  · {f}\x1b[0m");
                        }
                    }
                }
                PipelineEvent::PhaseRetrying { phase_id, title, reason, attempt } => {
                    if let Some(ref pb) = current_pb {
                        pb.set_message(format!(
                            "⚙️   Phase {phase_id}: {title}   \x1b[33m⟳ 重试中 (attempt {attempt} failed: {reason})…\x1b[0m"
                        ));
                    }
                }
                PipelineEvent::PhaseFailed { phase_id, title, reason } => {
                    if let Some(pb) = current_pb.take() {
                        pb.finish_with_message(format!(
                            "❌  Phase {phase_id}: {title}   \x1b[31m{reason}\x1b[0m"
                        ));
                    }
                }
                PipelineEvent::PipelineFailed { error } => {
                    if let Some(pb) = current_pb.take() {
                        pb.finish_with_message(format!("❌  {error}"));
                    }
                }
                PipelineEvent::NetworkError { category, message, role } => {
                    let (icon, label) = stage_label(role);
                    let cat_colour = match category.as_str() {
                        "request_timeout"  => "\x1b[33m",
                        "connection_error" => "\x1b[31m",
                        "rate_limited"     => "\x1b[35m",
                        _                  => "\x1b[33m",
                    };
                    eprintln!(
                        "  ⚠️   {icon}  {label}   {cat_colour}[{category}]\x1b[0m {message}"
                    );
                }
                PipelineEvent::RiskAssessed {
                    risk_level, reason, affected_areas,
                    breaking_change, security_implications, cr_focus, risk_unavailable,
                } => {
                    if risk_unavailable {
                        println!("  ⚠️   Risk assessment unavailable — proceeding without risk data\n");
                    } else {
                        let (colour, icon) = match risk_level.as_str() {
                            "high"   => ("\x1b[31m", "🚨"),
                            "medium" => ("\x1b[33m", "⚠️ "),
                            _        => ("\x1b[32m", "✅"),
                        };
                        println!("        {icon}  Level: {colour}{}\x1b[0m  {reason}", risk_level.to_uppercase());
                        if !affected_areas.is_empty() {
                            println!("        Affected: {}", affected_areas.join(" · "));
                        }
                        if breaking_change {
                            println!("        \x1b[31m⚡ Breaking change\x1b[0m");
                        }
                        if !security_implications.is_empty() {
                            println!("        🔒 Security: {security_implications}");
                        }
                        if !cr_focus.is_empty() {
                            println!("        👁  CR Focus: {cr_focus}");
                        }
                        println!();
                    }
                }
                PipelineEvent::ReviewCompleted {
                    approved, criteria_met, issues: _, security_concerns: _, recommendation,
                } => {
                    // Brief real-time summary only — full details printed by print_pipeline_result below.
                    let (icon, colour) = if approved && criteria_met {
                        ("✅", "\x1b[32m")
                    } else if approved {
                        ("⚠️ ", "\x1b[33m")
                    } else {
                        ("⚠️ ", "\x1b[33m")
                    };
                    println!("        {icon}  Review: {colour}{recommendation}\x1b[0m");
                    println!();
                }
                PipelineEvent::PipelineRetrying { reason, attempt } => {
                    if let Some(pb) = current_pb.take() {
                        pb.finish_and_clear();
                    }
                    eprintln!("  🔄  Pipeline restarting (attempt {attempt}): {reason}");
                }
                PipelineEvent::StageAborted { role, reason } => {
                    if let Some(pb) = current_pb.take() {
                        pb.finish_and_clear();
                    }
                    let (icon, label) = stage_label(role);
                    eprintln!("  ⚠️   {icon}  {label}   aborted: {reason}");
                }
                PipelineEvent::DriftDetected { kind, reason } => {
                    if let Some(ref pb) = current_pb {
                        pb.set_message(format!("  🔀  Drift detected [{kind}]: {reason}"));
                    } else {
                        eprintln!("  🔀  Drift detected [{kind}]: {reason}");
                    }
                }
            }
        }

        match pipeline.await {
            Ok(Ok(outputs)) => {
                println!();
                print_pipeline_result(&outputs);
            }
            Ok(Err(e)) => {
                error!("Pipeline failed: {e}");
                eprintln!("💥  Pipeline failed: {e}");
            }
            Err(e) => {
                error!("Pipeline task panicked: {e}");
                eprintln!("💥  Internal error: {e}");
            }
        }
        println!();
    }
}

// ──────────────────────────────────────────────
// Misc helpers
// ──────────────────────────────────────────────

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// Format a Unix timestamp as a human-readable string without pulling in chrono.
fn chrono_or_secs(secs: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let Ok(t) = UNIX_EPOCH.checked_add(Duration::from_secs(secs)).ok_or(()) else {
        return secs.to_string();
    };
    // Format as local time via SystemTime → FILETIME approximation
    // We avoid a heavy dep by just showing date components extracted manually.
    let since = std::time::SystemTime::now()
        .duration_since(t)
        .unwrap_or_default();
    let mins  = since.as_secs() / 60;
    let hours = mins / 60;
    let days  = hours / 24;
    if days == 0 && hours == 0 && mins < 2 {
        "just now".to_string()
    } else if days == 0 && hours == 0 {
        format!("{mins}m ago")
    } else if days == 0 {
        format!("{hours}h ago")
    } else if days < 7 {
        format!("{days}d ago")
    } else {
        // Fall back to epoch seconds for older dates
        format!("ts:{secs}")
    }
}
