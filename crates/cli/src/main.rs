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
//! harnesscode context init                # auto-generate AGENTS.md
//! ```

use clap::{Parser, Subcommand};
use harnesscode_core::{
    agents::{AgentOutput, AgentRole},
    config::{
        load_config, project_config_path, user_config_path,
        HarnessConfig, ProfileConfig, PROJECT_CONFIG_FILE,
    },
    controller::{
        ClarificationCallback, ClarificationRequest, ClarificationResolution, Controller,
        PipelineEvent, RequestContext,
    },
};
use indicatif::{ProgressBar, ProgressStyle};
use inquire::{Confirm, Select, Text};
use std::{collections::HashMap, time::Duration};
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
    /// Generate project context files
    Context {
        #[command(subcommand)]
        action: ContextAction,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Interactive wizard to create or update a config file
    Init,
    /// Print the currently resolved configuration (merged from all layers)
    Show,
}

#[derive(Subcommand, Debug)]
enum ContextAction {
    /// Auto-generate an AGENTS.md file for the current project
    Init,
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
// context init command
// ──────────────────────────────────────────────

fn cmd_context_init() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let content = harnesscode_core::context::agents_md::generate(&cwd);

    let dest = cwd.join("AGENTS.md");
    if dest.exists() {
        let overwrite = Confirm::new("AGENTS.md already exists. Overwrite?")
            .with_default(false)
            .prompt()
            .unwrap_or(false);
        if !overwrite {
            println!("Aborted.");
            return;
        }
    }

    match std::fs::write(&dest, &content) {
        Ok(()) => println!("✅  Generated {}", dest.display()),
        Err(e) => eprintln!("❌  Failed to write AGENTS.md: {e}"),
    }
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

        // ── Coder: colourised diff ────────────────────────────────────────────
        if output.role == AgentRole::Coder {
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

        // ── Reviewer: colour verdict ──────────────────────────────────────────
        if output.role == AgentRole::Reviewer {
            let verdict_colour = if output.success { "\x1b[32m" } else { "\x1b[31m" };
            println!("        {verdict_colour}Verdict: {}\x1b[0m", output.summary);

            let criteria_met = output.payload.get("criteria_met").and_then(|v| v.as_bool()).unwrap_or(true);
            let criteria_icon = if criteria_met { "\x1b[32m✅" } else { "\x1b[31m❌" };
            println!("        {criteria_icon}  Success criteria met\x1b[0m");

            if let Some(issues) = output.payload.get("issues").and_then(|i| i.as_array()) {
                if !issues.is_empty() {
                    println!("        Issues:");
                    for issue in issues {
                        println!("          • {}", issue.as_str().unwrap_or_default());
                    }
                }
            }
        }

        println!();
    }
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
            Commands::Context { action } => {
                match action {
                    ContextAction::Init => cmd_context_init(),
                }
                return;
            }
        }
    }

    // ── Interactive agent session ────────────────────────────────────────────
    print_banner();

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

    // ── LLM pipeline ─────────────────────────────────────────────────────────
    let llm = match harnesscode_core::config::provider_for_profile(cli.profile.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("❌  LLM configuration error: {e}");
            eprintln!("    Run `harnesscode config init` to set up your API credentials.");
            return;
        }
    };

    let controller = {
        let mut c = Controller::new(3, llm);
        // CLI flag > config file > default (100)
        let max_turns = cli.max_tool_turns
            .or_else(|| load_config().max_tool_turns);
        if let Some(turns) = max_turns {
            c = c.with_max_tool_turns(turns);
        }
        c
    };

    // Spawn the pipeline on a separate task; receive progress events on this thread.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PipelineEvent>(16);
    let task_clone = task.clone();
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
    let pipeline = tokio::spawn(async move {
        controller
            .run_with_request_context(
                &RequestContext::from_prompt(task_clone),
                Some(tx),
                None,
                Some(clarification_callback),
            )
            .await
    });

    // Label/icon for each agent role.
    fn stage_label(role: AgentRole) -> (&'static str, &'static str) {
        match role {
            AgentRole::Judge    => ("⚖️", "Judge   "),
            AgentRole::Scoper   => ("🧭", "Scoper  "),
            AgentRole::Planner  => ("🧠", "Planner "),
            AgentRole::Coder    => ("💻", "Coder   "),
            AgentRole::Risk     => ("🛡️", "Risk    "),
            AgentRole::Reviewer => ("🔍", "Reviewer"),
        }
    }

    let mut current_pb: Option<ProgressBar> = None;

    while let Some(event) = rx.recv().await {
        match event {
            PipelineEvent::StageStarted { role } => {
                // Finish any existing spinner before creating a new one.
                if let Some(pb) = current_pb.take() {
                    pb.finish_and_clear();
                }
                let (icon, label) = stage_label(role);
                let pb = make_spinner(&format!("{icon}  {label}   正在处理…"));
                current_pb = Some(pb);
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
                    ready_for_planner,
                    ready_for_scoper,
                    ask_user_clarification,
                );
                println!(
                    "    Criteria: goal_concrete={} constraints_stable={} history_resolves_refs={} repo_grounding_needed={} prior_scope_reusable={}",
                    goal_is_concrete,
                    constraints_are_stable,
                    history_resolves_references,
                    repository_grounding_needed,
                    prior_scope_can_be_reused,
                );
                for criterion in &skip_scoper_criteria_met {
                    println!("    • skip scoper: {criterion}");
                }
                for question in &clarifying_questions {
                    println!("    ? {question}");
                }
                println!();
            }
            PipelineEvent::ScopeReady {
                task_type,
                objective,
                unknowns,
                success_criteria,
                relevant_files,
                needs_user_clarification,
                clarifying_questions,
                ..
            } => {
                println!("\n  🧭  Problem Frame  ({})\n", task_type.to_uppercase());
                println!("    Objective: {objective}");
                for criterion in &success_criteria {
                    println!("    • {criterion}");
                }
                if !unknowns.is_empty() {
                    println!("\n  ❓  Unknowns:");
                    for item in &unknowns {
                        println!("      • {item}");
                    }
                }
                if needs_user_clarification && !clarifying_questions.is_empty() {
                    println!("\n  🗣️  Clarifying questions:");
                    for question in &clarifying_questions {
                        println!("      ? {question}");
                    }
                }
                if !relevant_files.is_empty() {
                    println!("\n  📁  Relevant files:");
                    for file in &relevant_files {
                        println!("      • {file}");
                    }
                }
                println!();
            }
            PipelineEvent::ClarificationRequested {
                source,
                objective,
                questions,
            } => {
                println!("\n  ❓  Clarification Requested by {source}\n");
                println!("    Objective: {objective}");
                for question in &questions {
                    println!("    ? {question}");
                }
                println!();
            }
            PipelineEvent::PlanReady { steps, affected_files, complexity } => {
                // The Planner spinner was already finished by StageCompleted.
                // Print the todo list directly to stdout.
                let complexity_colour = match complexity.as_str() {
                    "high"   => "\x1b[31m",
                    "medium" => "\x1b[33m",
                    _        => "\x1b[32m",
                };
                println!(
                    "\n  📋  Execution Plan  (complexity: {complexity_colour}{}\x1b[0m)\n",
                    complexity.to_uppercase()
                );
                for (i, step) in steps.iter().enumerate() {
                    println!("    {}. {step}", i + 1);
                }
                if !affected_files.is_empty() {
                    println!("\n  📁  Files to change:");
                    for f in &affected_files {
                        println!("      • {f}");
                    }
                }
                println!();
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
        }
    }

    // Collect the final result.
    let outputs = match pipeline.await {
        Ok(Ok(outputs)) => outputs,
        Ok(Err(e)) => {
            error!("Pipeline failed: {e}");
            eprintln!("💥  Pipeline failed: {e}");
            return;
        }
        Err(e) => {
            error!("Pipeline task panicked: {e}");
            eprintln!("💥  Internal error: {e}");
            return;
        }
    };

    println!();
    print_pipeline_result(&outputs);
}
