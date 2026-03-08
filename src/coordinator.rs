use crate::{
    config::TaskDef,
    facts::FactsStore,
    meta::{TaskMeta, TaskStatus},
    runner::{TaskRunReport, TaskRunner},
};
use anyhow::Result;
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub enum DispatchRequest {
    Task(TaskDef),
    Wave(Vec<TaskDef>),
    Seal,
}

struct PendingTask {
    def: TaskDef,
    barrier: HashSet<String>,
}

impl PendingTask {
    fn is_unblocked(&self, completed: &HashSet<String>) -> bool {
        self.barrier.is_subset(completed)
            && self
                .def
                .depends_on
                .iter()
                .all(|dep| completed.contains(dep))
    }
}

pub struct RunCoordinator {
    run_dir: PathBuf,
    runner: TaskRunner,
    cancel: CancellationToken,
    dispatch_rx: mpsc::Receiver<DispatchRequest>,
}

impl RunCoordinator {
    pub fn start(
        run_dir: &Path,
        runner: TaskRunner,
        cancel: CancellationToken,
    ) -> (Self, mpsc::Sender<DispatchRequest>) {
        let (dispatch_tx, dispatch_rx) = mpsc::channel(256);
        (
            Self {
                run_dir: run_dir.to_path_buf(),
                runner,
                cancel,
                dispatch_rx,
            },
            dispatch_tx,
        )
    }

    pub async fn run(mut self, initial_tasks: Vec<TaskDef>) -> bool {
        let log_dir = self.run_dir.join("logs");
        let facts_store = FactsStore::new(&self.run_dir);
        let mut join_set: JoinSet<TaskRunReport> = JoinSet::new();
        let mut pending = VecDeque::new();
        let mut accepted = HashSet::new();
        let mut running = HashSet::new();
        let mut completed = HashSet::new();
        let mut all_ok = true;
        let mut sealed = false;

        for task in initial_tasks {
            accepted.insert(task.name.clone());
            pending.push_back(PendingTask {
                def: task,
                barrier: HashSet::new(),
            });
        }

        loop {
            self.spawn_ready_tasks(
                &facts_store,
                &mut pending,
                &mut running,
                &completed,
                &mut join_set,
            );

            if join_set.is_empty() && pending.is_empty() && (sealed || self.dispatch_rx.is_closed())
            {
                break;
            }

            tokio::select! {
                biased;

                result = join_set.join_next(), if !join_set.is_empty() => {
                    match result {
                        Some(Ok(report)) => {
                            running.remove(&report.name);
                            completed.insert(report.name.clone());

                            match report.status {
                                TaskStatus::Failed => {
                                    all_ok = false;
                                    self.cancel.cancel();
                                }
                                TaskStatus::Cancelled => {
                                    all_ok = false;
                                }
                                _ => {}
                            }
                        }
                        Some(Err(_join_err)) => {
                            all_ok = false;
                            self.cancel.cancel();
                        }
                        None => {}
                    }
                }

                msg = self.dispatch_rx.recv(), if !sealed => {
                    match msg {
                        Some(DispatchRequest::Task(task)) => {
                            let pending_names = pending_names(&pending);
                            let barrier = if task.depends_on.is_empty() {
                                build_barrier(&running, &pending_names)
                            } else {
                                HashSet::new()
                            };
                            let newly_accepted_count = usize::from(accept_pending_task(
                                &mut accepted,
                                &mut pending,
                                task,
                                barrier,
                                &self.run_dir,
                                &log_dir,
                            ));
                            bump_run_task_count(&self.run_dir, newly_accepted_count);
                        }
                        Some(DispatchRequest::Wave(tasks)) => {
                            let pending_names = pending_names(&pending);
                            let barrier = build_barrier(&running, &pending_names);
                            let mut newly_accepted_count = 0;
                            for task in tasks {
                                newly_accepted_count += usize::from(accept_pending_task(
                                    &mut accepted,
                                    &mut pending,
                                    task,
                                    barrier.clone(),
                                    &self.run_dir,
                                    &log_dir,
                                ));
                            }
                            bump_run_task_count(&self.run_dir, newly_accepted_count);
                        }
                        Some(DispatchRequest::Seal) | None => {
                            self.dispatch_rx.close();
                            sealed = true;
                        }
                    }
                }

                _ = self.cancel.cancelled() => {
                    self.dispatch_rx.close();
                    while let Some(task) = pending.pop_front() {
                        write_cancelled_meta(
                            &log_dir,
                            &task.def,
                            "run cancelled before task started",
                        );
                    }
                    all_ok = false;
                    sealed = true;
                }
            }
        }

        all_ok
    }

    fn spawn_ready_tasks(
        &self,
        facts_store: &FactsStore,
        pending: &mut VecDeque<PendingTask>,
        running: &mut HashSet<String>,
        completed: &HashSet<String>,
        join_set: &mut JoinSet<TaskRunReport>,
    ) {
        if self.cancel.is_cancelled() {
            return;
        }

        let pending_count = pending.len();
        for _ in 0..pending_count {
            let Some(task) = pending.pop_front() else {
                break;
            };

            if task.is_unblocked(completed) {
                let mut task = task;
                let task_name = task.def.name.clone();
                let runner = self.runner.clone();
                let cancel = self.cancel.clone();

                if let Ok(Some(preamble)) = facts_store.build_preamble() {
                    task.def.prompt = format!("{}\n\n{}", preamble, task.def.prompt);
                }
                join_set.spawn(async move { runner.run_task(task.def, cancel, None).await });
                running.insert(task_name);
            } else {
                pending.push_back(task);
            }
        }
    }
}

pub fn validate_dispatch_name(name: &str, accepted: &HashSet<String>) -> Result<()> {
    crate::commands::serve::validate_task_name(name)?;
    anyhow::ensure!(
        !accepted.contains(name),
        "task name '{}' is already in this run",
        name
    );
    Ok(())
}

pub fn validate_dispatch_deps(depends_on: &[String], accepted: &HashSet<String>) -> Result<()> {
    for dep in depends_on {
        anyhow::ensure!(
            accepted.contains(dep),
            "depends_on references unknown task '{}'; only already-accepted tasks are allowed",
            dep
        );
    }
    Ok(())
}

pub fn build_barrier(running: &HashSet<String>, pending_names: &[String]) -> HashSet<String> {
    running
        .iter()
        .cloned()
        .chain(pending_names.iter().cloned())
        .collect()
}

fn pending_names(pending: &VecDeque<PendingTask>) -> Vec<String> {
    pending.iter().map(|task| task.def.name.clone()).collect()
}

fn accept_pending_task(
    accepted: &mut HashSet<String>,
    pending: &mut VecDeque<PendingTask>,
    task: TaskDef,
    barrier: HashSet<String>,
    run_dir: &Path,
    log_dir: &Path,
) -> bool {
    if !accepted.insert(task.name.clone()) {
        return false;
    }

    let meta = build_pending_meta(run_dir, &task);
    pending.push_back(PendingTask { def: task, barrier });
    let _ = meta.save(log_dir);
    true
}

fn build_pending_meta(_run_dir: &Path, task: &TaskDef) -> TaskMeta {
    let mut meta = TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
    meta.status = TaskStatus::Pending;
    meta.agent_id = task.agent_id.clone().unwrap_or_else(|| task.name.clone());
    meta.thread_id = task.thread_id.clone().unwrap_or_else(|| task.name.clone());
    meta.dispatched_at = Some(chrono::Local::now());
    // TODO: populate run_id here once RunMeta persists it.
    meta
}

fn bump_run_task_count(run_dir: &Path, newly_accepted_count: usize) {
    if newly_accepted_count == 0 {
        return;
    }

    if let Ok(mut run_meta) = crate::meta::RunMeta::load(run_dir) {
        run_meta.task_count += newly_accepted_count;
        let _ = run_meta.save(run_dir);
    }
}

fn write_cancelled_meta(log_dir: &Path, task: &TaskDef, reason: &str) {
    let meta_path = log_dir.join(format!("{}.meta.json", task.name));
    let mut meta = TaskMeta::load(&meta_path)
        .unwrap_or_else(|_| TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt));
    if meta.agent_id.is_empty() {
        meta.agent_id = task.agent_id.clone().unwrap_or_else(|| task.name.clone());
    }
    if meta.thread_id.is_empty() {
        meta.thread_id = task.thread_id.clone().unwrap_or_else(|| task.name.clone());
    }
    meta.status = TaskStatus::Cancelled;
    meta.error = Some(reason.to_string());
    meta.end_time = Some(chrono::Local::now());
    let _ = meta.save(log_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sandbox;

    fn task(name: &str) -> TaskDef {
        TaskDef {
            name: name.to_string(),
            cwd: PathBuf::from("/tmp"),
            prompt: "prompt".to_string(),
            sandbox: Sandbox::default(),
            agent_id: Some(format!("{name}-agent")),
            thread_id: Some(format!("{name}-thread")),
            output_schema: None,
            ephemeral: false,
            add_dirs: Vec::new(),
            ask_for_approval: "never".to_string(),
            config_overrides: Vec::new(),
            profile: None,
            model: None,
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn validate_name_rejects_duplicate() {
        let accepted: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        assert!(validate_dispatch_name("task_a", &accepted).is_err());
        assert!(validate_dispatch_name("task_b", &accepted).is_ok());
    }

    #[test]
    fn validate_name_rejects_forward_dep() {
        let accepted: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        let result = validate_dispatch_deps(&["task_b".to_string()], &accepted);
        assert!(result.is_err());
    }

    #[test]
    fn validate_name_allows_known_dep() {
        let accepted: std::collections::HashSet<String> =
            ["task_a".to_string()].into_iter().collect();
        let result = validate_dispatch_deps(&["task_a".to_string()], &accepted);
        assert!(result.is_ok());
    }

    #[test]
    fn barrier_snapshot_includes_running_and_pending() {
        let running: std::collections::HashSet<String> = ["r1".to_string()].into_iter().collect();
        let pending_names = vec!["p1".to_string()];
        let barrier = build_barrier(&running, &pending_names);
        assert!(barrier.contains("r1"));
        assert!(barrier.contains("p1"));
    }

    #[test]
    fn pending_meta_is_written_for_newly_accepted_task() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let mut accepted = HashSet::new();
        let mut pending = VecDeque::new();

        assert!(accept_pending_task(
            &mut accepted,
            &mut pending,
            task("dynamic-task"),
            HashSet::new(),
            dir.path(),
            &log_dir,
        ));

        let meta = TaskMeta::load(&log_dir.join("dynamic-task.meta.json")).unwrap();
        assert_eq!(meta.status, TaskStatus::Pending);
        assert_eq!(meta.agent_id, "dynamic-task-agent");
        assert_eq!(meta.thread_id, "dynamic-task-thread");
        assert!(meta.dispatched_at.is_some());
        assert!(meta.run_id.is_empty());
    }

    #[test]
    fn bump_run_task_count_updates_run_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        crate::meta::RunMeta {
            status: crate::meta::RunStatus::Running,
            task_count: 2,
            wave_count: 1,
            started_at: chrono::Local::now(),
            error: None,
        }
        .save(dir.path())
        .unwrap();

        bump_run_task_count(dir.path(), 3);

        let run_meta = crate::meta::RunMeta::load(dir.path()).unwrap();
        assert_eq!(run_meta.task_count, 5);
    }
}
