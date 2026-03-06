mod commands;
mod config;
mod dashboard;
mod event;
mod meta;
mod runner;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
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
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config, dashboard, dir } => {
            let base = PathBuf::from(&dir);
            let tasks_config = config::TasksConfig::load(&config)?;

            // Topological sort into waves (validates duplicates, deps, cycles)
            let waves = tasks_config.into_waves()?;
            let total_tasks: usize = waves.iter().map(|w| w.len()).sum();

            let runner = runner::TaskRunner::new(&base)?;
            let shutdown = CancellationToken::new();
            let log_dir = runner.log_dir().to_path_buf();

            // Pre-create Pending meta files for all tasks so dashboard shows full pipeline
            for (wave_idx, wave) in waves.iter().enumerate() {
                for task in wave {
                    let mut m = meta::TaskMeta::new(&task.name, &task.cwd, &task.prompt);
                    m.wave = Some(wave_idx as u32);
                    m.status = meta::TaskStatus::Pending;
                    let _ = m.save(&log_dir);
                }
            }

            println!("Launching {} tasks in {} wave(s)...\n", total_tasks, waves.len());

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

            let mut all_reports = Vec::new();
            let mut wave_failed = false;
            let mut sigint_received = false;

            for (wave_idx, wave) in waves.into_iter().enumerate() {
                if wave_failed || sigint_received {
                    // Mark remaining tasks as cancelled
                    let reason = if sigint_received {
                        "cancelled by user"
                    } else {
                        "skipped: previous wave had failures"
                    };
                    for task in &wave {
                        let mut m = meta::TaskMeta::new(&task.name, &task.cwd, &task.prompt);
                        m.wave = Some(wave_idx as u32);
                        m.status = meta::TaskStatus::Cancelled;
                        m.error = Some(reason.into());
                        m.end_time = Some(chrono::Local::now());
                        let _ = m.save(&log_dir);
                        all_reports.push(runner::TaskRunReport {
                            name: task.name.clone(),
                            status: meta::TaskStatus::Cancelled,
                            error: Some(reason.into()),
                        });
                    }
                    continue;
                }

                let wave_size = wave.len();
                println!(
                    "-- Wave {} ({} task{}, parallel) --",
                    wave_idx,
                    wave_size,
                    if wave_size == 1 { "" } else { "s" }
                );

                // Track spawned task names so we can fix up stale metas after panics
                let mut wave_task_names: Vec<String> = Vec::new();
                let mut set = JoinSet::new();
                for task in wave {
                    wave_task_names.push(task.name.clone());
                    let runner = runner.clone();
                    let cancel = shutdown.clone();
                    let w = wave_idx as u32;
                    set.spawn(async move {
                        runner.run_task(task, cancel, Some(w)).await
                    });
                }

                // Collect results for this wave
                let mut reported_names = std::collections::HashSet::new();
                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c(), if !sigint_received => {
                            sigint_received = true;
                            eprintln!("\nReceived Ctrl+C, cancelling all tasks...");
                            shutdown.cancel();
                        }
                        result = set.join_next() => {
                            match result {
                                Some(Ok(report)) => {
                                    if report.status == meta::TaskStatus::Failed {
                                        wave_failed = true;
                                    }
                                    reported_names.insert(report.name.clone());
                                    all_reports.push(report);
                                }
                                Some(Err(e)) => {
                                    eprintln!("Task panicked: {}", e);
                                    wave_failed = true;
                                }
                                None => break, // All tasks in this wave done
                            }
                        }
                    }
                }

                // Fix up any tasks that panicked (reported via JoinError, no TaskRunReport)
                for name in &wave_task_names {
                    if !reported_names.contains(name) {
                        // Task panicked — update meta on disk to terminal state
                        let meta_path = log_dir.join(format!("{}.meta.json", name));
                        if let Ok(mut m) = meta::TaskMeta::load(&meta_path) {
                            m.status = meta::TaskStatus::Failed;
                            m.error = Some("task panicked".into());
                            m.end_time = Some(chrono::Local::now());
                            let _ = m.save(&log_dir);
                        }
                        all_reports.push(runner::TaskRunReport {
                            name: name.clone(),
                            status: meta::TaskStatus::Failed,
                            error: Some("task panicked".into()),
                        });
                    }
                }
            }

            // Stop dashboard
            if let Some(handle) = dashboard_handle {
                shutdown.cancel();
                handle.await.ok();
            }

            // Print summary
            println!("\n{}", "=".repeat(50));
            println!("  SUMMARY ({} tasks)", total_tasks);
            println!("{}", "=".repeat(50));
            for report in &all_reports {
                let icon = match report.status {
                    meta::TaskStatus::Done => "\x1b[32m✓\x1b[0m",
                    meta::TaskStatus::Failed => "\x1b[31m✗\x1b[0m",
                    meta::TaskStatus::Cancelled => "\x1b[35m⊘\x1b[0m",
                    _ => "?",
                };
                print!("  {} {}", icon, report.name);
                if let Some(ref err) = report.error {
                    print!("  -- {}", err);
                }
                println!();
            }
            println!("\n  Results: {}/outputs/*.md", dir);

            let all_ok = all_reports.iter().all(|r| r.status == meta::TaskStatus::Done);
            if !all_ok {
                return Err(anyhow::anyhow!("one or more tasks failed"));
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
            commands::tail::tail_follow(&log_file, &meta_file).await?;
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
