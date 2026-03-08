use crate::config::TaskDef;
use crate::event::CodexEvent;
use crate::meta::{TaskMeta, TaskStatus};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio_util::sync::CancellationToken;

/// Every N events, flush meta to disk (balance IO vs realtime).
const META_UPDATE_INTERVAL: u64 = 20;

/// Time to wait after SIGTERM before escalating to SIGKILL.
const SIGKILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn codex_bin() -> String {
    std::env::var("CODEX_BIN").unwrap_or_else(|_| "/opt/homebrew/bin/codex".to_string())
}

#[derive(Clone)]
pub struct TaskRunner {
    output_dir: PathBuf,
    log_dir: PathBuf,
}

impl TaskRunner {
    pub fn new(base_dir: &Path) -> Result<Self> {
        let output_dir = base_dir.join("outputs");
        let log_dir = base_dir.join("logs");
        std::fs::create_dir_all(&output_dir)
            .with_context(|| format!("failed to create output dir: {}", output_dir.display()))?;
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("failed to create log dir: {}", log_dir.display()))?;
        Ok(Self {
            output_dir,
            log_dir,
        })
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub async fn run_task(
        &self,
        task: TaskDef,
        cancel: CancellationToken,
        wave: Option<u32>,
    ) -> TaskRunReport {
        let mut meta = TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
        if let Some(existing) = self.load_existing_meta(&task.name) {
            meta.run_id = existing.run_id;
            meta.wave = wave.or(existing.wave);
            meta.dispatched_at = existing.dispatched_at;
        } else {
            meta.wave = wave;
        }
        meta.agent_id = task.agent_id.clone().unwrap_or_else(|| task.name.clone());
        meta.thread_id = task.thread_id.clone().unwrap_or_else(|| task.name.clone());
        meta.status = TaskStatus::Running;
        if let Err(e) = meta.save(&self.log_dir) {
            eprintln!("[{}] warn: failed to write Running meta: {}", task.name, e);
        }

        match self.run_task_inner(&task, &mut meta, &cancel).await {
            Ok(()) => {
                if let Err(e) = meta.save(&self.log_dir) {
                    eprintln!("[{}] warn: failed to write final meta: {}", task.name, e);
                }
                TaskRunReport {
                    name: task.name.clone(),
                    status: meta.status.clone(),
                    error: meta.error.clone(),
                }
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    meta.status = TaskStatus::Cancelled;
                    meta.error = Some("cancelled by user".into());
                } else {
                    meta.status = TaskStatus::Failed;
                    meta.error = Some(e.to_string());
                }
                meta.end_time = Some(chrono::Local::now());
                if let Err(save_err) = meta.save(&self.log_dir) {
                    eprintln!(
                        "[{}] warn: failed to write Failed meta: {}",
                        task.name, save_err
                    );
                }
                TaskRunReport {
                    name: task.name.clone(),
                    status: meta.status.clone(),
                    error: meta.error.clone(),
                }
            }
        }
    }

    pub async fn resume_task(
        &self,
        meta: &mut TaskMeta,
        session_id: &str,
        prompt: &str,
    ) -> Result<()> {
        meta.session_id = Some(session_id.to_string());
        meta.prompt_preview = prompt.chars().take(150).collect();
        meta.status = TaskStatus::Running;
        meta.pid = None;
        meta.start_time = chrono::Local::now();
        meta.end_time = None;
        meta.last_action.clear();
        meta.exit_code = None;
        meta.error = None;
        meta.save(&self.log_dir)
            .context("failed to write resume Running meta")?;

        match self.resume_task_inner(meta, session_id, prompt).await {
            Ok(()) => {
                meta.save(&self.log_dir)
                    .context("failed to write resume final meta")?;
                Ok(())
            }
            Err(e) => {
                meta.status = TaskStatus::Failed;
                meta.error = Some(e.to_string());
                meta.end_time = Some(chrono::Local::now());
                if let Err(save_err) = meta.save(&self.log_dir) {
                    eprintln!(
                        "[{}] warn: failed to write resume Failed meta: {}",
                        meta.name, save_err
                    );
                }
                Err(e)
            }
        }
    }

    async fn run_task_inner(
        &self,
        task: &TaskDef,
        meta: &mut TaskMeta,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let result_file = self.output_dir.join(format!("{}.md", task.name));
        let log_file = self.log_dir.join(format!("{}.jsonl", task.name));
        let stderr_file = self.log_dir.join(format!("{}.stderr.log", task.name));

        // Build command
        let mut cmd = Command::new(codex_bin());
        cmd.args(["exec", "-C"]);
        cmd.arg(&task.cwd);
        cmd.args(["-s", &task.sandbox.to_string()]);
        cmd.args([
            "-o",
            result_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("output file path is not valid UTF-8"))?,
        ]);
        cmd.args(["--json", "--skip-git-repo-check"]);
        if let Some(ref model) = task.model {
            cmd.args(["-m", model]);
        }
        if let Some(ref output_schema) = task.output_schema {
            cmd.arg("--output-schema");
            cmd.arg(output_schema);
        }
        if task.ephemeral {
            cmd.arg("--ephemeral");
        }
        for add_dir in &task.add_dirs {
            cmd.arg("--add-dir");
            cmd.arg(add_dir);
        }
        cmd.args(["-a", &task.ask_for_approval]);
        for kv in &task.config_overrides {
            cmd.args(["-c", kv]);
        }
        if let Some(ref profile) = task.profile {
            cmd.args(["-p", profile]);
        }
        cmd.arg(&task.prompt);
        prepare_command(&mut cmd);

        let mut child = cmd.spawn().context("failed to spawn codex process")?;
        meta.pid = child.id();
        meta.save(&self.log_dir)
            .context("failed to persist pid to meta")?;

        // Drain stderr to file (prevents pipe buffer deadlock).
        // Truncate on each run so reruns start fresh.
        let stderr = child.stderr.take().expect("stderr captured");
        let stderr_drain = spawn_stderr_drain(stderr, stderr_file.clone(), false);

        // Stream stdout JSONL. Truncate on each run so reruns start fresh.
        let stdout = child.stdout.take().expect("stdout captured");
        let mut reader = BufReader::new(stdout).lines();
        let mut log_writer = open_log_writer(&log_file, false).await?;

        loop {
            tokio::select! {
                // Bias toward stdout: if EOF and cancel are both ready, drain stdout first
                // so we don't misclassify a completed task as cancelled.
                biased;

                result = reader.next_line() => {
                    match result {
                        Ok(Some(line)) => {
                            log_writer.write_all(line.as_bytes()).await?;
                            log_writer.write_all(b"\n").await?;

                            meta.events_count += 1;

                            if let Some(event) = CodexEvent::parse(&line) {
                                update_meta_from_event(meta, &event);
                            }

                            if meta.events_count % META_UPDATE_INTERVAL == 0 {
                                if let Err(e) = meta.save(&self.log_dir) {
                                    eprintln!("[{}] warn: periodic meta save failed: {}", meta.name, e);
                                }
                            }

                            // Check cancel between lines so chatty tasks still respond to Ctrl+C
                            if cancel.is_cancelled() {
                                kill_and_wait(&mut child, meta.pid).await;
                                meta.status = TaskStatus::Cancelled;
                                meta.end_time = Some(chrono::Local::now());
                                break;
                            }
                        }
                        Ok(None) => break, // EOF
                        Err(e) => {
                            meta.error = Some(format!("stdout read error: {}", e));
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    kill_and_wait(&mut child, meta.pid).await;
                    meta.status = TaskStatus::Cancelled;
                    meta.end_time = Some(chrono::Local::now());
                    break;
                }
            }
        }

        // Wait for stderr drain to finish (ignore join error — drain task is best-effort)
        if let Err(e) = stderr_drain.await {
            eprintln!("[{}] warn: stderr drain task failed: {}", meta.name, e);
        }

        // Wait for process exit (if not already killed by cancel path)
        if meta.status != TaskStatus::Cancelled {
            let exit_status = child
                .wait()
                .await
                .context("failed to wait for codex process")?;
            meta.exit_code = exit_status.code();
            meta.end_time = Some(chrono::Local::now());
            // If we already recorded an error (e.g. stdout read failure), keep Failed
            if meta.error.is_some() {
                meta.status = TaskStatus::Failed;
            } else {
                meta.status = if exit_status.success() {
                    TaskStatus::Done
                } else {
                    TaskStatus::Failed
                };
            }
        }

        Ok(())
    }

    async fn resume_task_inner(
        &self,
        meta: &mut TaskMeta,
        session_id: &str,
        prompt: &str,
    ) -> Result<()> {
        let result_file = self.output_dir.join(format!("{}.md", meta.name));
        let log_file = self.log_dir.join(format!("{}.jsonl", meta.name));
        let stderr_file = self.log_dir.join(format!("{}.stderr.log", meta.name));

        let mut cmd = Command::new(codex_bin());
        cmd.current_dir(&meta.cwd);
        cmd.arg("exec");
        cmd.arg("resume");
        cmd.arg(session_id);
        cmd.arg(prompt);
        cmd.arg("--json");
        cmd.arg("-o");
        cmd.arg(
            result_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("output file path is not valid UTF-8"))?,
        );
        prepare_command(&mut cmd);

        let mut child = cmd
            .spawn()
            .context("failed to spawn codex resume process")?;
        meta.pid = child.id();
        meta.save(&self.log_dir)
            .context("failed to persist resume pid to meta")?;

        let stderr = child.stderr.take().expect("stderr captured");
        let stderr_drain = spawn_stderr_drain(stderr, stderr_file.clone(), true);

        let stdout = child.stdout.take().expect("stdout captured");
        let mut reader = BufReader::new(stdout).lines();
        let mut log_writer = open_log_writer(&log_file, true).await?;

        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    log_writer.write_all(line.as_bytes()).await?;
                    log_writer.write_all(b"\n").await?;

                    meta.events_count += 1;

                    if let Some(event) = CodexEvent::parse(&line) {
                        update_meta_from_event(meta, &event);
                    }

                    if meta.events_count % META_UPDATE_INTERVAL == 0 {
                        if let Err(e) = meta.save(&self.log_dir) {
                            eprintln!("[{}] warn: periodic meta save failed: {}", meta.name, e);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    meta.error = Some(format!("stdout read error: {}", e));
                    break;
                }
            }
        }

        if let Err(e) = stderr_drain.await {
            eprintln!("[{}] warn: stderr drain task failed: {}", meta.name, e);
        }

        let exit_status = child
            .wait()
            .await
            .context("failed to wait for codex resume process")?;
        meta.exit_code = exit_status.code();
        meta.end_time = Some(chrono::Local::now());
        if meta.error.is_some() {
            meta.status = TaskStatus::Failed;
        } else {
            meta.status = if exit_status.success() {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
        }

        Ok(())
    }

    fn load_existing_meta(&self, task_name: &str) -> Option<TaskMeta> {
        let meta_path = self.log_dir.join(format!("{}.meta.json", task_name));
        TaskMeta::load(&meta_path).ok()
    }
}

fn prepare_command(cmd: &mut Command) {
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Put child in its own process group for clean shutdown.
    #[cfg(unix)]
    {
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

async fn open_log_writer(path: &Path, append: bool) -> Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options
        .open(path)
        .await
        .with_context(|| format!("failed to open event log: {}", path.display()))
}

fn spawn_stderr_drain(
    stderr: ChildStderr,
    stderr_file: PathBuf,
    append: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).write(true);
        if append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        let mut writer = match options.open(&stderr_file).await {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!(
                    "warn: could not open stderr log {}: {}",
                    stderr_file.display(),
                    e
                );
                None
            }
        };
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(ref mut w) = writer {
                if w.write_all(line.as_bytes()).await.is_err() || w.write_all(b"\n").await.is_err()
                {
                    // drain continues even if write fails
                }
            }
        }
    })
}

/// Graceful shutdown: SIGTERM → wait up to SIGKILL_TIMEOUT → SIGKILL → wait.
/// Always reaps the child even on error paths to prevent zombies.
async fn kill_and_wait(child: &mut Child, pid: Option<u32>) {
    // If the process already exited (e.g. EOF and cancel raced), skip signalling.
    match child.try_wait() {
        Ok(Some(_)) => return, // already reaped
        Ok(None) => {}         // still running — proceed with shutdown
        Err(_) => {}           // try_wait failed — proceed anyway, wait() will reap
    }

    #[cfg(unix)]
    if let Some(pid) = pid {
        // Send SIGTERM to the entire process group.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    }

    // Give the process a moment to exit cleanly, then escalate to SIGKILL.
    match tokio::time::timeout(SIGKILL_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => {
            // Process exited cleanly within timeout — nothing more to do.
        }
        Ok(Err(e)) => {
            // wait() itself failed — still escalate to SIGKILL to avoid leaving it running.
            eprintln!("warn: wait() failed during graceful shutdown: {}", e);
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            child.kill().await.ok();
            child.wait().await.ok();
        }
        Err(_elapsed) => {
            // Timed out — escalate to SIGKILL.
            #[cfg(unix)]
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            // Force kill via tokio handle as fallback (handles non-Unix or pgid miss).
            child.kill().await.ok();
            // Final wait to reap zombie.
            child.wait().await.ok();
        }
    }
}

fn update_meta_from_event(meta: &mut TaskMeta, event: &CodexEvent) {
    match event {
        CodexEvent::SessionStart { session_id } => {
            meta.session_id = Some(session_id.clone());
        }
        CodexEvent::TokenCount {
            input_tokens,
            output_tokens,
        } => {
            meta.input_tokens = *input_tokens;
            meta.output_tokens = *output_tokens;
        }
        CodexEvent::ReadFile { file_path } => {
            meta.files_read += 1;
            let filename = file_path.rsplit('/').next().unwrap_or(file_path);
            meta.last_action = format!("READ {}", filename);
        }
        CodexEvent::ExecCommand { command } => {
            meta.commands_run += 1;
            let short: String = command.chars().take(50).collect();
            meta.last_action = format!("EXEC {}", short);
        }
        CodexEvent::TaskComplete => {
            meta.last_action = "COMPLETED".to_string();
        }
        CodexEvent::Other => {}
    }
}

#[derive(Debug)]
pub struct TaskRunReport {
    pub name: String,
    pub status: TaskStatus,
    pub error: Option<String>,
}
