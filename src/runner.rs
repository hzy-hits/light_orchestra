use crate::config::TaskDef;
use crate::event::CodexEvent;
use crate::meta::{TaskMeta, TaskStatus};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
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
        Ok(Self { output_dir, log_dir })
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub async fn run_task(&self, task: TaskDef, cancel: CancellationToken, wave: Option<u32>) -> TaskRunReport {
        let mut meta = TaskMeta::new(&task.name, &task.cwd, &task.prompt);
        meta.wave = wave;
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
                    eprintln!("[{}] warn: failed to write Failed meta: {}", task.name, save_err);
                }
                TaskRunReport {
                    name: task.name.clone(),
                    status: meta.status.clone(),
                    error: meta.error.clone(),
                }
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
        cmd.args(["exec", "-C", &task.cwd, "-s", &task.sandbox]);
        cmd.args([
            "-o",
            result_file
                .to_str()
                .context("output file path is not valid UTF-8")?,
        ]);
        cmd.args(["--json", "--skip-git-repo-check"]);
        if let Some(ref model) = task.model {
            cmd.args(["-m", model]);
        }
        cmd.arg(&task.prompt);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Put child in its own process group for clean shutdown
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd.spawn().context("failed to spawn codex process")?;
        meta.pid = child.id();
        meta.save(&self.log_dir)
            .context("failed to persist pid to meta")?;

        // Drain stderr to file (prevents pipe buffer deadlock).
        // Truncate on each run so reruns start fresh.
        let stderr = child.stderr.take().expect("stderr captured");
        let stderr_drain = {
            let stderr_file = stderr_file.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                // Truncate (create/truncate) so reruns don't accumulate output
                let mut writer = match tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&stderr_file)
                    .await
                {
                    Ok(f) => Some(f),
                    Err(e) => {
                        eprintln!("warn: could not open stderr log {}: {}", stderr_file.display(), e);
                        None
                    }
                };
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some(ref mut w) = writer {
                        if w.write_all(line.as_bytes()).await.is_err()
                            || w.write_all(b"\n").await.is_err()
                        {
                            // drain continues even if write fails
                        }
                    }
                }
            })
        };

        // Stream stdout JSONL. Truncate on each run so reruns start fresh.
        let stdout = child.stdout.take().expect("stdout captured");
        let mut reader = BufReader::new(stdout).lines();
        let mut log_writer = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_file)
            .await
            .with_context(|| format!("failed to open event log: {}", log_file.display()))?;

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
        CodexEvent::TokenCount { input_tokens, output_tokens } => {
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
        CodexEvent::Other(_) => {}
    }
}

#[derive(Debug)]
pub struct TaskRunReport {
    pub name: String,
    pub status: TaskStatus,
    pub error: Option<String>,
}
