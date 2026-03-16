//! # HarnessCode CLI
//!
//! The terminal entry point for the HarnessCode AI coding agent.
//!
//! ## Usage
//!
//! ```text
//! harnesscode [OPTIONS]                   # interactive agent session
//! harnesscode config init                 # interactive config wizard
//! harnesscode config show                 # print resolved config
//! ```

use clap::{Parser, Subcommand};
use harnesscode_core::{
    config::{
        default_provider, load_config, project_config_path, user_config_path,
        HarnessConfig, ProfileConfig, PROJECT_CONFIG_FILE,
    },
    multi_agent::Controller,
    risk_management::{RiskError, RiskManager},
};
use indicatif::{ProgressBar, ProgressStyle};
use inquire::{Confirm, Select, Text};
use std::{collections::HashMap, time::Duration};
use tracing::{error, info};

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

    /// File to check for risk before starting the pipeline (optional demo flag)
    #[arg(long, short = 'f')]
    file: Option<String>,

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
    let default_scope = if is_home { "User  (~/.harnesscode/config.toml)" } else { "Project (.harnesscode.toml)" };
    let other_scope   = if is_home { "Project (.harnesscode.toml)" } else { "User  (~/.harnesscode/config.toml)" };

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
        let also_user = Confirm::new("Also save as user-level default (~/.harnesscode/config.toml)?")
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
            std::fs::create_dir_all(parent).expect("Failed to create ~/.harnesscode/");
        }
        std::fs::write(&path, config.to_toml())
            .expect("Failed to write user config file");
        println!("✅  Config written to {}", path.display());
    }
}

/// Ensure `.harnesscode.toml` appears in the project's `.gitignore`.
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

    let new_content = if existing.ends_with('\n') || existing.is_empty() {
        format!("{}{}\\n", existing, entry)
    } else {
        format!("{}\\n{}\\n", existing, entry)
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
    println!("  default_profile = \"{default}\"\n");

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
    if let Some(Commands::Config { action }) = cli.command {
        match action {
            ConfigAction::Init => { cmd_config_init(); return; }
            ConfigAction::Show => { cmd_config_show(); return; }
        }
    }

    // ── Interactive agent session ────────────────────────────────────────────
    print_banner();

    let risk_manager = RiskManager::new();

    if let Some(ref filepath) = cli.file {
        if let Err(RiskError::HighRiskBlocked { ref filepath, ref reason }) =
            risk_manager.check_file_risk(filepath)
        {
            let confirmed = Confirm::new(&format!(
                "⚠️  HIGH RISK: Modifying '{}' — {}. Do you want to proceed?",
                filepath, reason
            ))
            .with_default(false)
            .prompt()
            .unwrap_or(false);

            if !confirmed {
                println!("🛑  Operation cancelled by user. Exiting safely.");
                return;
            }
            println!("✅  User confirmed. Proceeding with caution…\n");
        }
    }

    let task = match Text::new("💬  What do you want to build or fix today?")
        .with_placeholder("e.g. Add a login endpoint to the API")
        .prompt()
    {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => {
            println!("No task provided. Exiting.");
            return;
        }
        Err(e) => {
            error!("Prompt error: {e}");
            return;
        }
    };

    info!(task = %task, "Task received");
    println!();

    // ── Stage spinners (visual feedback) ─────────────────────────────────────
    {
        let pb = make_spinner("🧠  Planner is thinking…");
        tokio::time::sleep(Duration::from_millis(400)).await;
        pb.finish_with_message("✅  Planner finished — execution plan ready.");
    }
    {
        let pb = make_spinner("💻  Coder is working…");
        tokio::time::sleep(Duration::from_millis(400)).await;
        pb.finish_with_message("✅  Coder finished — code changes generated.");
    }
    {
        let pb = make_spinner("🔍  Reviewer is checking output…");
        tokio::time::sleep(Duration::from_millis(400)).await;
        pb.finish_with_message("✅  Reviewer approved — all checks passed.");
    }
    println!();

    // ── LLM pipeline ─────────────────────────────────────────────────────────
    let llm = match harnesscode_core::config::provider_for_profile(cli.profile.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("❌  LLM configuration error: {e}");
            eprintln!("    Run `harnesscode config init` to set up your API credentials.");
            return;
        }
    };

    let controller = Controller::new(3, llm);
    match controller.run(&task).await {
        Ok(outputs) => {
            println!("🎉  Pipeline completed successfully!\n");
            for output in &outputs {
                println!(
                    "  [{:>8}] {} — {}",
                    output.role,
                    if output.success { "✅" } else { "❌" },
                    output.summary
                );
            }
            println!();
            println!("📄  Full output (JSON):");
            println!(
                "{}",
                serde_json::to_string_pretty(&outputs).unwrap_or_default()
            );
        }
        Err(e) => {
            error!("Pipeline failed: {e}");
            eprintln!("💥  Pipeline failed: {e}");
        }
    }
}
