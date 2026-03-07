/// Quiet execution core: run waves without printing to stdout.
/// Used by both `run` (via a wrapper that adds printing/signals) and `serve`.
use crate::{config::TaskDef, meta, runner::TaskRunner};
use std::{collections::HashSet, path::Path};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Execute task waves silently. Returns `true` if all tasks succeeded.
pub async fn run_waves_quiet(
    waves: Vec<Vec<TaskDef>>,
    runner: TaskRunner,
    log_dir: &Path,
    cancel: CancellationToken,
) -> bool {
    let mut all_ok = true;
    let mut wave_failed = false;

    for (wave_idx, wave) in waves.into_iter().enumerate() {
        if wave_failed || cancel.is_cancelled() {
            let reason = if cancel.is_cancelled() {
                "cancelled by user"
            } else {
                "skipped: previous wave had failures"
            };
            for task in &wave {
                let mut m =
                    meta::TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
                m.wave = Some(wave_idx as u32);
                m.status = meta::TaskStatus::Cancelled;
                m.error = Some(reason.into());
                m.end_time = Some(chrono::Local::now());
                let _ = m.save(log_dir);
            }
            all_ok = false;
            continue;
        }

        let mut wave_task_names: Vec<String> = Vec::new();
        let mut set = JoinSet::new();
        for task in wave {
            wave_task_names.push(task.name.clone());
            let r = runner.clone();
            let c = cancel.clone();
            let w = wave_idx as u32;
            set.spawn(async move { r.run_task(task, c, Some(w)).await });
        }

        let mut reported: HashSet<String> = HashSet::new();
        while let Some(result) = set.join_next().await {
            match result {
                Ok(report) => {
                    match report.status {
                        meta::TaskStatus::Failed => {
                            wave_failed = true;
                            all_ok = false;
                        }
                        // Cancelled counts as non-ok even though it doesn't cause
                        // downstream waves to skip (cancel token handles that).
                        meta::TaskStatus::Cancelled => {
                            all_ok = false;
                        }
                        _ => {}
                    }
                    reported.insert(report.name);
                }
                Err(_join_err) => {
                    wave_failed = true;
                    all_ok = false;
                }
            }
        }

        // Fix up any tasks that panicked (JoinError — no report written).
        for name in &wave_task_names {
            if !reported.contains(name) {
                let meta_path = log_dir.join(format!("{}.meta.json", name));
                if let Ok(mut m) = meta::TaskMeta::load(&meta_path) {
                    m.status = meta::TaskStatus::Failed;
                    m.error = Some("task panicked".into());
                    m.end_time = Some(chrono::Local::now());
                    let _ = m.save(log_dir);
                }
                wave_failed = true;
                all_ok = false;
            }
        }
    }

    all_ok
}
