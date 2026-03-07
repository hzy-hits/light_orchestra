use crate::{config, execution, meta, runner::TaskRunner};
use anyhow::Result;
use rmcp::{
    ErrorData as McpError,
    ServerHandler,
    handler::server::{
        tool::ToolRouter,
        wrapper::Parameters,
        tool::ToolCallContext,
    },
    model::*,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::{Path, PathBuf}, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── Parameter types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct StartRunParams {
    /// YAML content of the tasks configuration (full text, not a file path)
    tasks_yaml: String,
    /// Directory for outputs/ and logs/ (created if absent)
    run_dir: String,
    /// Remove existing task state in run_dir before starting (default: false)
    #[serde(default)]
    clean: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RunDirParam {
    /// Base directory passed to start_run
    run_dir: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Task name (must match [A-Za-z0-9._-])
    task_name: String,
    /// Byte offset to start reading from (default 0)
    offset: Option<u64>,
    /// Maximum bytes to return (default 65536)
    max_bytes: Option<u64>,
}

// ── Result types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct StartRunResult {
    run_dir: String,
    task_count: usize,
    wave_count: usize,
    poll_interval_ms: u64,
}

#[derive(Debug, Serialize)]
struct TaskStatusInfo {
    name: String,
    status: String,
    wave: Option<u32>,
    tokens_in: u64,
    tokens_out: u64,
    last_action: String,
    error: Option<String>,
    exit_code: Option<i32>,
    has_output: bool,
}

#[derive(Debug, Serialize)]
struct GetStatusResult {
    /// "running" | "done" | "failed" | "not_found"
    run_status: String,
    tasks: Vec<TaskStatusInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadFileResult {
    content: String,
    next_offset: u64,
    eof: bool,
}

#[derive(Debug, Serialize)]
struct CancelResult {
    /// "cancel_requested" | "already_terminal" | "not_found"
    state: String,
}

// ── Server state ─────────────────────────────────────────────────────────────

struct ActiveRun {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

struct ServerState {
    runs: Mutex<HashMap<PathBuf, ActiveRun>>,
}

// ── Server ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CodexParServer {
    state: Arc<ServerState>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl CodexParServer {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ServerState {
                runs: Mutex::new(HashMap::new()),
            }),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Start parallel Codex tasks from an inline YAML string. Returns immediately — poll get_status for completion.")]
    async fn start_run(&self, p: Parameters<StartRunParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;

        let run_dir = canonicalize_or_create(&p.run_dir)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        {
            let runs = self.state.runs.lock().await;
            if let Some(active) = runs.get(&run_dir) {
                if !active.handle.is_finished() {
                    return Err(McpError::invalid_params(
                        format!("run_dir {:?} already has an active run; cancel it first", run_dir),
                        None,
                    ));
                }
            }
        }

        let tasks_config = config::TasksConfig::from_str(&p.tasks_yaml)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let waves = tasks_config
            .into_waves()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let task_count: usize = waves.iter().map(|w| w.len()).sum();
        let wave_count = waves.len();
        let log_dir = run_dir.join("logs");
        let out_dir = run_dir.join("outputs");

        if log_dir.exists() {
            if p.clean {
                std::fs::remove_dir_all(&log_dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            } else {
                let has_stale = std::fs::read_dir(&log_dir)
                    .map(|d| {
                        d.filter_map(|e| e.ok())
                            .any(|e| e.file_name().to_string_lossy().ends_with(".meta.json"))
                    })
                    .unwrap_or(false);
                if has_stale {
                    return Err(McpError::invalid_params(
                        "run_dir contains existing task state; pass clean=true to remove it".to_string(),
                        None,
                    ));
                }
            }
        }

        std::fs::create_dir_all(&log_dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        meta::RunMeta {
            status: meta::RunStatus::Running,
            task_count,
            wave_count,
            started_at: chrono::Local::now(),
            error: None,
        }
        .save(&run_dir)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        for (wave_idx, wave) in waves.iter().enumerate() {
            for task in wave {
                let mut m =
                    meta::TaskMeta::new(&task.name, &task.cwd.to_string_lossy(), &task.prompt);
                m.wave = Some(wave_idx as u32);
                m.status = meta::TaskStatus::Pending;
                m.save(&log_dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        }

        let runner = TaskRunner::new(&run_dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let cancel = CancellationToken::new();
        let cancel_bg = cancel.clone();
        let log_dir_bg = log_dir.clone();
        let run_dir_bg = run_dir.clone();
        let state = Arc::clone(&self.state);

        let handle = tokio::spawn(async move {
            let all_ok =
                execution::run_waves_quiet(waves, runner, &log_dir_bg, cancel_bg).await;
            if let Ok(mut rm) = meta::RunMeta::load(&run_dir_bg) {
                rm.status = if all_ok {
                    meta::RunStatus::Done
                } else {
                    // Collect the first failure message from task metas to surface in run.meta.json.
                    rm.error = collect_first_task_error(&log_dir_bg);
                    meta::RunStatus::Failed
                };
                let _ = rm.save(&run_dir_bg);
            }
            state.runs.lock().await.remove(&run_dir_bg);
        });

        self.state
            .runs
            .lock()
            .await
            .insert(run_dir.clone(), ActiveRun { cancel, handle });

        ok_json(&StartRunResult {
            run_dir: run_dir.to_string_lossy().into_owned(),
            task_count,
            wave_count,
            poll_interval_ms: 2000,
        })
    }

    #[tool(description = "Get status of all tasks in a run. Poll until run_status is 'done' or 'failed'. Sorted by (wave, name).")]
    async fn get_status(&self, p: Parameters<RunDirParam>) -> Result<CallToolResult, McpError> {
        let run_dir = PathBuf::from(&p.0.run_dir);
        let run_meta = meta::RunMeta::load(&run_dir).ok();
        let run_status = match &run_meta {
            None => "not_found",
            Some(rm) => match rm.status {
                meta::RunStatus::Running => "running",
                meta::RunStatus::Done => "done",
                meta::RunStatus::Failed => "failed",
            },
        };

        if run_meta.is_none() {
            return ok_json(&GetStatusResult { run_status: "not_found".into(), tasks: vec![] });
        }

        let log_dir = run_dir.join("logs");
        let out_dir = run_dir.join("outputs");
        let mut tasks: Vec<TaskStatusInfo> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let name = fname.to_string_lossy();
                if !name.ends_with(".meta.json") || name.ends_with(".meta.json.tmp") {
                    continue;
                }
                if let Ok(m) = meta::TaskMeta::load(&entry.path()) {
                    let has_output = out_dir.join(format!("{}.md", m.name)).exists();
                    tasks.push(TaskStatusInfo {
                        name: m.name,
                        status: m.status.to_string(),
                        wave: m.wave,
                        tokens_in: m.input_tokens,
                        tokens_out: m.output_tokens,
                        last_action: m.last_action,
                        error: m.error,
                        exit_code: m.exit_code,
                        has_output,
                    });
                }
            }
        }

        tasks.sort_by(|a, b| a.wave.cmp(&b.wave).then_with(|| a.name.cmp(&b.name)));

        ok_json(&GetStatusResult { run_status: run_status.into(), tasks })
    }

    #[tool(description = "Read task output file. Supports chunked reading via offset/max_bytes for large files.")]
    async fn read_output(&self, p: Parameters<ReadFileParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        validate_task_name(&p.task_name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let path = PathBuf::from(&p.run_dir)
            .join("outputs")
            .join(format!("{}.md", p.task_name));
        read_file_chunked(&path, p.offset, p.max_bytes)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    #[tool(description = "Read task stderr log. Supports chunked reading via offset/max_bytes.")]
    async fn read_stderr(&self, p: Parameters<ReadFileParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        validate_task_name(&p.task_name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let path = PathBuf::from(&p.run_dir)
            .join("logs")
            .join(format!("{}.stderr.log", p.task_name));
        read_file_chunked(&path, p.offset, p.max_bytes)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))
    }

    #[tool(description = "Cancel an active run. Returns promptly; tasks converge to cancelled state.")]
    async fn cancel_run(&self, p: Parameters<RunDirParam>) -> Result<CallToolResult, McpError> {
        let run_dir = PathBuf::from(&p.0.run_dir);
        let canon = run_dir.canonicalize().unwrap_or(run_dir);
        let mut runs = self.state.runs.lock().await;
        let state_str = if let Some(active) = runs.get(&canon) {
            if active.handle.is_finished() {
                runs.remove(&canon);
                "already_terminal"
            } else {
                active.cancel.cancel();
                "cancel_requested"
            }
        } else {
            "not_found"
        };
        ok_json(&CancelResult { state: state_str.into() })
    }
}

impl ServerHandler for CodexParServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "codex-par MCP server: run parallel Codex tasks without MCP deadlock. \
                 Call start_run, poll get_status, then read_output.",
            )
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let ctx = ToolCallContext::new(self, request, context);
        async move { self.tool_router.call(ctx).await }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = self.tool_router.list_all();
        async move { Ok(ListToolsResult { tools, ..Default::default() }) }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn serve_command() -> Result<()> {
    use rmcp::ServiceExt;
    let server = CodexParServer::new();
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server init failed: {}", e))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ok_json<T: Serialize>(v: &T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(v).expect("serialize"),
    )]))
}

fn canonicalize_or_create(path_str: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path_str);
    std::fs::create_dir_all(&path)?;
    Ok(path.canonicalize()?)
}

pub fn validate_task_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "task name cannot be empty");
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    anyhow::ensure!(
        valid,
        "task name '{}' contains invalid characters (only [A-Za-z0-9._-] allowed)",
        name
    );
    anyhow::ensure!(
        name != "." && name != "..",
        "task name '{}' is not a valid file stem",
        name
    );
    Ok(())
}

fn collect_first_task_error(log_dir: &Path) -> Option<String> {
    let Ok(entries) = std::fs::read_dir(log_dir) else { return None };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let name = fname.to_string_lossy();
        if !name.ends_with(".meta.json") || name.ends_with(".meta.json.tmp") {
            continue;
        }
        if let Ok(m) = meta::TaskMeta::load(&entry.path()) {
            if m.status == meta::TaskStatus::Failed {
                if let Some(e) = m.error {
                    return Some(e);
                }
            }
        }
    }
    None
}

fn read_file_chunked(
    path: &Path,
    offset: Option<u64>,
    max_bytes: Option<u64>,
) -> Result<CallToolResult> {
    use std::io::{Read, Seek, SeekFrom};

    if !path.exists() {
        return Err(anyhow::anyhow!("file not found: {}", path.display()));
    }
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let start = offset.unwrap_or(0);
    let max = max_bytes.unwrap_or(65536);
    let to_read = max.min(file_len.saturating_sub(start)) as usize;
    let mut buf = vec![0u8; to_read];
    if to_read > 0 {
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
    }
    let next_offset = start + to_read as u64;
    ok_json(&ReadFileResult {
        content: String::from_utf8_lossy(&buf).into_owned(),
        next_offset,
        eof: next_offset >= file_len,
    })
    .map_err(|e| anyhow::anyhow!("{}", e.message))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validate_task_name_ok() {
        for name in &["foo", "foo-bar", "foo.bar", "foo_bar", "A1.b-c_d"] {
            assert!(validate_task_name(name).is_ok(), "expected ok for {name:?}");
        }
    }

    #[test]
    fn validate_task_name_rejects() {
        for name in &["", ".", "..", "../evil", "foo/bar", "foo bar", "foo\nbar"] {
            assert!(validate_task_name(name).is_err(), "expected err for {name:?}");
        }
    }

    #[test]
    fn read_file_chunked_full() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.md");
        std::fs::write(&path, b"hello world").unwrap();

        let result = read_file_chunked(&path, None, None).unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        let parsed: ReadFileResult = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.content, "hello world");
        assert!(parsed.eof);
        assert_eq!(parsed.next_offset, 11);
    }

    #[test]
    fn read_file_chunked_with_offset() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.md");
        std::fs::write(&path, b"hello world").unwrap();

        let result = read_file_chunked(&path, Some(6), Some(5)).unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        let parsed: ReadFileResult = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.content, "world");
        assert!(parsed.eof);
        assert_eq!(parsed.next_offset, 11);
    }

    #[test]
    fn read_file_chunked_partial() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.md");
        std::fs::write(&path, b"hello world").unwrap();

        let result = read_file_chunked(&path, Some(0), Some(5)).unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        let parsed: ReadFileResult = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.content, "hello");
        assert!(!parsed.eof);
        assert_eq!(parsed.next_offset, 5);
    }

    #[test]
    fn read_file_chunked_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.md");
        assert!(read_file_chunked(&path, None, None).is_err());
    }

    #[test]
    fn run_meta_round_trip() {
        let dir = TempDir::new().unwrap();
        let rm = meta::RunMeta {
            status: meta::RunStatus::Running,
            task_count: 3,
            wave_count: 2,
            started_at: chrono::Local::now(),
            error: None,
        };
        rm.save(dir.path()).unwrap();
        let loaded = meta::RunMeta::load(dir.path()).unwrap();
        assert_eq!(loaded.status, meta::RunStatus::Running);
        assert_eq!(loaded.task_count, 3);
        assert_eq!(loaded.wave_count, 2);
        assert!(loaded.error.is_none());
    }

    #[test]
    fn canonicalize_or_create_idempotent() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("a").join("b");
        let p1 = canonicalize_or_create(sub.to_str().unwrap()).unwrap();
        let p2 = canonicalize_or_create(sub.to_str().unwrap()).unwrap();
        assert_eq!(p1, p2);
    }
}
