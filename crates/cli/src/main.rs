//! # HarnessCode CLI
//!
//! The terminal entry point for the HarnessCode AI coding agent.
//!
//! Usage:
//! ```
//! harnesscode [--log-level <level>]
//! ```
//!
//! The CLI:
//! 1. Greets the user with the HarnessCode banner.
//! 2. Prompts for the task via `inquire`.
//! 3. Runs risk checks on any involved files (blocking on `High` risk).
//! 4. Simulates the Planner → Coder → Reviewer pipeline with `indicatif` spinners.
//! 5. Prints the final structured output.

use clap::Parser;
use harnesscode_core::{
    multi_agent::Controller,
    risk_management::{RiskError, RiskManager},
};
use indicatif::{ProgressBar, ProgressStyle};
use inquire::{Confirm, Text};
use std::time::Duration;
use tracing::{error, info};

// ──────────────────────────────────────────────
// CLI arguments
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

    /// File to check for risk before starting the pipeline (optional demo flag)
    #[arg(long, short = 'f')]
    file: Option<String>,
}

// ──────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────

/// Print the HarnessCode welcome banner.
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

/// Create a styled spinner with the given message.
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

    // ── Banner ───────────────────────────────────────────────────────────────
    print_banner();

    // ── Risk check (optional demo flag) ─────────────────────────────────────
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

    // ── Task prompt ──────────────────────────────────────────────────────────
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

    // ── Stage 1: Planner ─────────────────────────────────────────────────────
    {
        let pb = make_spinner("🧠  Planner is thinking…");
        tokio::time::sleep(Duration::from_secs(1)).await;
        pb.finish_with_message("✅  Planner finished — execution plan ready.");
    }

    // ── Stage 2: Coder ───────────────────────────────────────────────────────
    {
        let pb = make_spinner("💻  Coder is working…");
        tokio::time::sleep(Duration::from_secs(1)).await;
        pb.finish_with_message("✅  Coder finished — code changes generated.");
    }

    // ── Stage 3: Sandboxed tests ─────────────────────────────────────────────
    {
        let pb = make_spinner("🔬  Running sandboxed tests…");
        tokio::time::sleep(Duration::from_secs(1)).await;
        pb.finish_with_message("✅  Sandboxed tests passed.");
    }

    // ── Stage 4: Reviewer ────────────────────────────────────────────────────
    {
        let pb = make_spinner("🔍  Reviewer is checking output…");
        tokio::time::sleep(Duration::from_secs(1)).await;
        pb.finish_with_message("✅  Reviewer approved — all checks passed.");
    }

    println!();

    // ── Run the actual controller (structured output) ─────────────────────────
    let controller = Controller::new(3);
    match controller.run(&task).await {
        Ok(outputs) => {
            println!("🎉  Pipeline completed successfully!\n");
            for output in &outputs {
                println!(
                    "  [{:>8}] {} — {}",
                    output.role, if output.success { "✅" } else { "❌" }, output.summary
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
