use crate::{config, dashboard, meta, runner};
use anyhow::Result;
use std::path::Path;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Execute all tasks from a YAML config in dependency waves.
/// Returns `true` if all tasks succeeded, `false` if any failed or were cancelled.
pub async fn run_command(config_path: &str, base: &Path, show_dashboard: bool) -> Result<bool> {
    let tasks_config = config::TasksConfig::load(config_path)?;
    let waves = tasks_config.into_waves()?;
    let total_tasks: usize = waves.iter().map(|w| w.len()).sum();

    let task_runner = runner::TaskRunner::new(base)?;
    let shutdown = CancellationToken::new();
    let log_dir = task_runner.log_dir().to_path_buf();

    // Pre-create Pending meta files for all tasks so the dashboard shows the full pipeline.
    for (wave_idx, wave) in waves.iter().enumerate() {
        for task in wave {
            let mut m = meta::TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
            m.wave = Some(wave_idx as u32);
            m.status = meta::TaskStatus::Pending;
            let _ = m.save(&log_dir);
        }
    }

    println!("Launching {} tasks in {} wave(s)...\n", total_tasks, waves.len());

    // Optionally spawn dashboard.
    let dashboard_handle = if show_dashboard {
        let ld = log_dir.clone();
        let cancel = shutdown.clone();
        Some(tokio::spawn(async move {
            dashboard::watch_until_cancelled(&ld, 1, cancel).await
        }))
    } else {
        None
    };

    let mut all_reports: Vec<runner::TaskRunReport> = Vec::new();
    let mut wave_failed = false;
    let mut sigint_received = false;

    for (wave_idx, wave) in waves.into_iter().enumerate() {
        if wave_failed || sigint_received {
            // Mark remaining tasks as cancelled without running them.
            let reason = if sigint_received {
                "cancelled by user"
            } else {
                "skipped: previous wave had failures"
            };
            for task in &wave {
                let mut m = meta::TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
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

        // Track names so we can fix up stale metas after panics.
        let mut wave_task_names: Vec<String> = Vec::new();
        let mut set = JoinSet::new();
        for task in wave {
            wave_task_names.push(task.name.clone());
            let r = task_runner.clone();
            let cancel = shutdown.clone();
            let w = wave_idx as u32;
            set.spawn(async move { r.run_task(task, cancel, Some(w)).await });
        }

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
                        None => break,
                    }
                }
            }
        }

        // Fix up any tasks that panicked (no TaskRunReport written).
        for name in &wave_task_names {
            if !reported_names.contains(name) {
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

    // Stop dashboard.
    if let Some(handle) = dashboard_handle {
        shutdown.cancel();
        handle.await.ok();
    }

    // Print summary.
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
    println!("\n  Results: {}/outputs/*.md", base.display());

    let all_ok = all_reports.iter().all(|r| r.status == meta::TaskStatus::Done);
    Ok(all_ok)
}
