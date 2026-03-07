use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Run-level metadata ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RunMeta {
    pub status: RunStatus,
    pub task_count: usize,
    pub wave_count: usize,
    pub started_at: DateTime<Local>,
    pub error: Option<String>,
}

impl RunMeta {
    /// Atomic save to `{run_dir}/run.meta.json`.
    pub fn save(&self, run_dir: &Path) -> anyhow::Result<()> {
        let final_path = run_dir.join("run.meta.json");
        let tmp_path = run_dir.join("run.meta.json.tmp");
        std::fs::write(&tmp_path, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    pub fn load(run_dir: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(run_dir.join("run.meta.json"))?;
        Ok(serde_json::from_str(&content)?)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TaskMeta {
    pub name: String,
    #[serde(default)]
    pub wave: Option<u32>,
    pub cwd: String,
    pub prompt_preview: String,
    pub status: TaskStatus,
    pub pid: Option<u32>,
    pub start_time: DateTime<Local>,
    pub end_time: Option<DateTime<Local>>,
    pub events_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub files_read: u32,
    pub commands_run: u32,
    pub last_action: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled)
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "pending"),
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Done => write!(f, "done"),
            TaskStatus::Failed => write!(f, "failed"),
            TaskStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl TaskMeta {
    pub fn new(name: &str, cwd: &str, prompt: &str) -> Self {
        Self {
            name: name.to_string(),
            wave: None,
            cwd: cwd.to_string(),
            prompt_preview: prompt.chars().take(150).collect(),
            status: TaskStatus::Pending,
            pid: None,
            start_time: Local::now(),
            end_time: None,
            events_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            files_read: 0,
            commands_run: 0,
            last_action: String::new(),
            exit_code: None,
            error: None,
        }
    }

    /// Atomic save: write to .tmp then rename, so readers never see partial JSON.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let final_path = dir.join(format!("{}.meta.json", self.name));
        let tmp_path = dir.join(format!("{}.meta.json.tmp", self.name));
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let meta: Self = serde_json::from_str(&content)?;
        Ok(meta)
    }

    /// Mark task as failed with error info and save immediately.
    pub fn fail(&mut self, dir: &Path, error: String) {
        self.status = TaskStatus::Failed;
        self.error = Some(error);
        self.end_time = Some(Local::now());
        let _ = self.save(dir);
    }
}
