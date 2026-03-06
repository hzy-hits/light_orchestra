mod config;
mod dashboard;
mod event;
mod meta;
mod runner;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "codex-par", about = "Parallel Codex task runner with live dashboard")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch all tasks in parallel
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config, dashboard, dir } => {
            let base = PathBuf::from(&dir);
            let tasks_config = config::TasksConfig::load(&config)?;

            // Validate: no duplicate task names
            let mut seen = std::collections::HashSet::new();
            for task in &tasks_config.tasks {
                anyhow::ensure!(
                    seen.insert(&task.name),
                    "duplicate task name: {}",
                    task.name
                );
            }

            let runner = runner::TaskRunner::new(&base)?;
            let shutdown = CancellationToken::new();
            let log_dir = runner.log_dir().to_path_buf();

            println!("Launching {} tasks in parallel...\n", tasks_config.tasks.len());

            // Spawn all tasks
            let mut set = JoinSet::new();
            for task in tasks_config.tasks {
                let runner = runner.clone();
                let cancel = shutdown.clone();
                set.spawn(async move {
                    runner.run_task(task, cancel).await
                });
            }

            // Optionally spawn dashboard
            let dashboard_handle = if dashboard {
                let ld = log_dir.clone();
                let cancel = shutdown.clone();
                Some(tokio::spawn(async move {
                    dashboard::watch_until_cancelled(&ld, 1, cancel).await
                }))
            } else {
                None
            };

            // Collect results, handle Ctrl+C
            let mut reports = Vec::new();
            let mut interrupted = false;

            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c(), if !interrupted => {
                        interrupted = true;
                        eprintln!("\nReceived Ctrl+C, cancelling all tasks...");
                        shutdown.cancel();
                    }
                    result = set.join_next() => {
                        match result {
                            Some(Ok(report)) => reports.push(report),
                            Some(Err(e)) => eprintln!("Task panicked: {}", e),
                            None => break, // All tasks done
                        }
                    }
                }
            }

            // Wait for dashboard to finish
            if let Some(handle) = dashboard_handle {
                handle.await.ok();
            }

            // Print summary
            println!("\n{}", "═".repeat(50));
            println!("  SUMMARY");
            println!("{}", "═".repeat(50));
            for report in &reports {
                let icon = match report.status {
                    meta::TaskStatus::Done => "\x1b[32m✓\x1b[0m",
                    meta::TaskStatus::Failed => "\x1b[31m✗\x1b[0m",
                    meta::TaskStatus::Cancelled => "\x1b[35m⊘\x1b[0m",
                    _ => "?",
                };
                print!("  {} {}", icon, report.name);
                if let Some(ref err) = report.error {
                    print!("  — {}", err);
                }
                println!();
            }
            println!("\n  Results: {}/outputs/*.md", dir);

            let all_ok = reports.iter().all(|r| r.status == meta::TaskStatus::Done);
            if !all_ok {
                std::process::exit(1);
            }
        }
        Commands::Status { dir, watch } => {
            let log_dir = PathBuf::from(&dir).join("logs");
            if let Some(interval) = watch {
                dashboard::watch(&log_dir, interval).await?;
            } else {
                dashboard::render_once(&log_dir)?;
            }
        }
        Commands::Tail { name, dir } => {
            let log_file = PathBuf::from(&dir).join("logs").join(format!("{}.jsonl", name));
            let meta_file = PathBuf::from(&dir).join("logs").join(format!("{}.meta.json", name));
            tail_follow(&log_file, &meta_file).await?;
        }
        Commands::Clean { dir } => {
            let base = PathBuf::from(&dir);
            std::fs::remove_dir_all(base.join("outputs")).ok();
            std::fs::remove_dir_all(base.join("logs")).ok();
            println!("Cleaned outputs/ and logs/");
        }
    }

    Ok(())
}

/// Tail with follow: reads existing lines, then polls for new ones until task completes.
async fn tail_follow(log_path: &PathBuf, meta_path: &PathBuf) -> anyhow::Result<()> {
    use crate::event::CodexEvent;
    use tokio::io::{AsyncBufReadExt, BufReader};

    if !log_path.exists() {
        anyhow::bail!("Log file not found: {}", log_path.display());
    }

    let mut file = tokio::fs::File::open(log_path).await?;
    let mut reader = BufReader::new(&mut file);

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // EOF — check if task is still running
            if meta_path.exists() {
                if let Ok(meta) = meta::TaskMeta::load(meta_path) {
                    if meta.status.is_terminal() {
                        println!("\n--- Task {} ({}) ---", meta.name, meta.status);
                        break;
                    }
                }
            }
            // Not done yet, wait and retry
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }

        let line = line.trim_end();
        if let Some(evt) = CodexEvent::parse(line) {
            let ts = CodexEvent::extract_timestamp(line);
            match evt {
                CodexEvent::ReadFile { file_path } => {
                    println!("[{}] READ  {}", ts, file_path);
                }
                CodexEvent::ExecCommand { command } => {
                    println!("[{}] EXEC  {}", ts, command);
                }
                CodexEvent::TokenCount { input_tokens, .. } => {
                    println!("[{}] TOKENS {:.0}K", ts, input_tokens as f64 / 1000.0);
                }
                CodexEvent::TaskComplete => {
                    println!("[{}] === TASK COMPLETE ===", ts);
                }
                CodexEvent::Other(_) => {}
            }
        }
    }
    Ok(())
}
