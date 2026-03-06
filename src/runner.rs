use crate::config::TaskDef;
use crate::event::CodexEvent;
use crate::meta::{TaskMeta, TaskStatus};
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Every N events, flush meta to disk (balance IO vs realtime).
const META_UPDATE_INTERVAL: u64 = 20;

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
        std::fs::create_dir_all(&output_dir)?;
        std::fs::create_dir_all(&log_dir)?;
        Ok(Self { output_dir, log_dir })
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub async fn run_task(&self, task: TaskDef, cancel: CancellationToken) -> TaskRunReport {
        let mut meta = TaskMeta::new(&task.name, &task.cwd, &task.prompt);
        meta.status = TaskStatus::Running;
        let _ = meta.save(&self.log_dir);

        match self.run_task_inner(&task, &mut meta, &cancel).await {
            Ok(()) => {
                meta.save(&self.log_dir).ok();
                TaskRunReport {
                    name: task.name.clone(),
                    status: meta.status.clone(),
                    error: None,
                }
            }
            Err(e) => {
                // Ensure meta is always updated on failure
                if cancel.is_cancelled() {
                    meta.status = TaskStatus::Cancelled;
                    meta.error = Some("cancelled by user".into());
                } else {
                    meta.status = TaskStatus::Failed;
                    meta.error = Some(e.to_string());
                }
                meta.end_time = Some(chrono::Local::now());
                meta.save(&self.log_dir).ok();
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
        cmd.args(["-o", result_file.to_str().unwrap()]);
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
            unsafe { cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            }); }
        }

        let mut child = cmd.spawn()?;
        meta.pid = child.id();
        meta.save(&self.log_dir)?;

        // Drain stderr to file (prevents pipe buffer deadlock)
        let stderr = child.stderr.take().expect("stderr captured");
        let stderr_drain = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            let mut writer = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stderr_file)
                .await
                .ok();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(ref mut w) = writer {
                    let _ = w.write_all(line.as_bytes()).await;
                    let _ = w.write_all(b"\n").await;
                }
            }
        });

        // Stream stdout JSONL
        let stdout = child.stdout.take().expect("stdout captured");
        let mut reader = BufReader::new(stdout).lines();
        let mut log_writer = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .await?;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // Kill the entire process group
                    #[cfg(unix)]
                    if let Some(pid) = meta.pid {
                        unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
                    }
                    child.kill().await.ok();
                    meta.status = TaskStatus::Cancelled;
                    meta.end_time = Some(chrono::Local::now());
                    break;
                }
                result = reader.next_line() => {
                    match result {
                        Ok(Some(line)) => {
                            // Always persist raw line first
                            log_writer.write_all(line.as_bytes()).await?;
                            log_writer.write_all(b"\n").await?;

                            meta.events_count += 1;

                            // Parse and update stats
                            if let Some(event) = CodexEvent::parse(&line) {
                                update_meta_from_event(meta, &event);
                            }

                            if meta.events_count % META_UPDATE_INTERVAL == 0 {
                                meta.save(&self.log_dir)?;
                            }
                        }
                        Ok(None) => break, // EOF
                        Err(e) => {
                            meta.error = Some(format!("stdout read error: {}", e));
                            break;
                        }
                    }
                }
            }
        }

        // Wait for stderr drain to finish
        stderr_drain.await.ok();

        // Wait for process exit (if not already killed)
        if meta.status != TaskStatus::Cancelled {
            let exit_status = child.wait().await?;
            meta.exit_code = exit_status.code();
            meta.end_time = Some(chrono::Local::now());
            meta.status = if exit_status.success() {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
        }

        Ok(())
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
