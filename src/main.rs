mod commands;
mod config;
mod dashboard;
mod event;
mod execution;
mod meta;
mod runner;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "codex-par", about = "Parallel Codex task runner with live dashboard")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch tasks in dependency waves (parallel within each wave)
    Run {
        /// Path to tasks YAML config
        config: String,
        /// Also show live dashboard
        #[arg(long)]
        dashboard: bool,
        /// Base directory for outputs and logs
        #[arg(short, long, default_value = ".")]
        dir: String,
    },
    /// Show dashboard (reads logs/ directory)
    Status {
        /// Base directory containing logs/
        #[arg(short, long, default_value = ".")]
        dir: String,
        /// Watch mode: auto-refresh every N seconds
        #[arg(short, long)]
        watch: Option<u64>,
    },
    /// Tail a specific task's event log (follows new output)
    Tail {
        /// Task name
        name: String,
        /// Base directory containing logs/
        #[arg(short, long, default_value = ".")]
        dir: String,
    },
    /// Clean outputs and logs
    Clean {
        #[arg(short, long, default_value = ".")]
        dir: String,
    },
    /// Run as MCP server over stdio (JSON-RPC 2.0)
    Serve,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config, dashboard, dir } => {
            match commands::run::run_command(&config, &PathBuf::from(&dir), dashboard).await {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(e) => {
                    eprintln!("Error: {:#}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Status { dir, watch } => {
            let log_dir = PathBuf::from(&dir).join("logs");
            match commands::status::status_command(&log_dir, watch).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {:#}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Tail { name, dir } => {
            let log_file = PathBuf::from(&dir).join("logs").join(format!("{}.jsonl", name));
            let meta_file = PathBuf::from(&dir).join("logs").join(format!("{}.meta.json", name));
            match commands::tail::tail_follow(&log_file, &meta_file).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {:#}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Clean { dir } => {
            match commands::clean::clean_command(&PathBuf::from(&dir)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {:#}", e);
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Serve => {
            match commands::serve::serve_command().await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {:#}", e);
                    ExitCode::FAILURE
                }
            }
        }
    }
}
