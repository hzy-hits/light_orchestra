use crate::meta::{TaskMeta, TaskStatus};
use anyhow::Result;
use chrono::Local;
use crossterm::{cursor, execute, terminal};
use std::io::Write;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub fn render_once(log_dir: &Path) -> Result<()> {
    let mut metas = collect_metas(log_dir)?;
    // Sort by (wave, name) for deterministic display order across platforms.
    // read_dir() order is unspecified, so we must sort explicitly.
    metas.sort_by(|a, b| a.wave.cmp(&b.wave).then_with(|| a.name.cmp(&b.name)));

    let mut out = std::io::stdout();
    execute!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

    let now = Local::now().format("%H:%M:%S");
    let total = metas.len();
    let done = metas
        .iter()
        .filter(|m| m.status == TaskStatus::Done)
        .count();
    let running = metas
        .iter()
        .filter(|m| m.status == TaskStatus::Running)
        .count();
    let failed = metas
        .iter()
        .filter(|m| m.status == TaskStatus::Failed)
        .count();

    writeln!(out, "{}", "=".repeat(72))?;
    writeln!(
        out,
        "  CODEX PARALLEL DASHBOARD  |  {}  |  {}/{} done, {} running, {} failed",
        now, done, total, running, failed
    )?;
    writeln!(out, "{}\n", "=".repeat(72))?;

    // Group by wave
    let mut wave_groups: std::collections::BTreeMap<u32, Vec<&TaskMeta>> =
        std::collections::BTreeMap::new();
    let mut no_wave: Vec<&TaskMeta> = Vec::new();
    for meta in &metas {
        match meta.wave {
            Some(w) => wave_groups.entry(w).or_default().push(meta),
            None => no_wave.push(meta),
        }
    }

    // Render tasks without wave info (v1 compat)
    for meta in &no_wave {
        render_task_line(&mut out, meta)?;
    }

    // Render wave groups
    for (wave_idx, tasks) in &wave_groups {
        let wave_done = tasks
            .iter()
            .filter(|m| m.status == TaskStatus::Done)
            .count();
        writeln!(
            out,
            "  -- Wave {} ({}/{} done) --",
            wave_idx,
            wave_done,
            tasks.len()
        )?;
        for meta in tasks {
            render_task_line(&mut out, meta)?;
        }
    }

    writeln!(out, "{}", "-".repeat(72))?;
    writeln!(
        out,
        "  Results: outputs/*.md  |  Logs: logs/*.jsonl  |  Ctrl+C to cancel"
    )?;
    out.flush()?;
    Ok(())
}

fn render_task_line(out: &mut impl Write, meta: &TaskMeta) -> Result<()> {
    let icon = match meta.status {
        TaskStatus::Running => "\x1b[33m⟳\x1b[0m",
        TaskStatus::Done => "\x1b[32m✓\x1b[0m",
        TaskStatus::Failed => "\x1b[31m✗\x1b[0m",
        TaskStatus::Pending => "\x1b[90m◯\x1b[0m",
        TaskStatus::Cancelled => "\x1b[35m⊘\x1b[0m",
    };

    let duration = if let Some(end) = meta.end_time {
        let dur = end - meta.start_time;
        format!("{}m{}s", dur.num_minutes(), dur.num_seconds() % 60)
    } else if meta.status == TaskStatus::Running {
        let dur = Local::now() - meta.start_time;
        format!("{}m{}s", dur.num_minutes(), dur.num_seconds() % 60)
    } else {
        "-".to_string()
    };

    let tokens_k = meta.input_tokens as f64 / 1000.0;

    writeln!(out, "    {} {}", icon, meta.name)?;
    writeln!(
        out,
        "      Status: {:<10} Duration: {:<10} Events: {}",
        meta.status, duration, meta.events_count
    )?;
    writeln!(
        out,
        "      Tokens: {:.0}K in / {:.0}K out    Files: {}  Cmds: {}",
        tokens_k,
        meta.output_tokens as f64 / 1000.0,
        meta.files_read,
        meta.commands_run
    )?;
    writeln!(out, "      Last: {}", truncate_utf8(&meta.last_action, 60))?;
    if let Some(ref err) = meta.error {
        writeln!(
            out,
            "      \x1b[31mError: {}\x1b[0m",
            truncate_utf8(err, 60)
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// Watch mode with cancellation support. Exits when all tasks are terminal or cancelled.
pub async fn watch_until_cancelled(
    log_dir: &Path,
    interval_secs: u64,
    cancel: CancellationToken,
) -> Result<()> {
    loop {
        render_once(log_dir)?;

        tokio::select! {
            _ = cancel.cancelled() => {
                render_once(log_dir).ok();
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval_secs)) => {}
        }

        // Exit if all tasks finished
        let metas = collect_metas(log_dir)?;
        if !metas.is_empty() && metas.iter().all(|m| m.status.is_terminal()) {
            render_once(log_dir)?;
            println!("\n  All tasks finished.");
            break;
        }
    }
    Ok(())
}

/// Standalone watch (for `codex-par status --watch`), no cancellation token.
pub async fn watch(log_dir: &Path, interval_secs: u64) -> Result<()> {
    let cancel = CancellationToken::new();
    // Cancel on Ctrl+C
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        c.cancel();
    });
    watch_until_cancelled(log_dir, interval_secs, cancel).await
}

fn collect_metas(log_dir: &Path) -> Result<Vec<TaskMeta>> {
    let mut metas = Vec::new();
    if !log_dir.exists() {
        return Ok(metas);
    }
    for entry in std::fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && path.to_str().unwrap_or("").contains(".meta.")
        {
            // Silently skip partial/corrupt files (atomic write prevents most cases)
            if let Ok(meta) = TaskMeta::load(&path) {
                metas.push(meta);
            }
        }
    }
    Ok(metas)
}

/// UTF-8 safe truncation — never panics on multi-byte characters.
fn truncate_utf8(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}
