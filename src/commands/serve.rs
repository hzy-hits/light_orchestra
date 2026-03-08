use crate::{
    config,
    facts::FactsStore,
    message,
    message::{MessageEnvelope, MessageType},
    meta,
    runner::TaskRunner,
};
use anyhow::{Context, Result};
use rmcp::{
    handler::server::{tool::ToolCallContext, tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_router, ErrorData as McpError, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
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
    /// If true, keep the dispatch channel open for dispatch_task/dispatch_wave calls.
    /// If false (default), the run auto-seals after initial tasks complete.
    #[serde(default)]
    dynamic: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RunDirParam {
    /// Base directory passed to start_run
    run_dir: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskRefParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Task name (must match [A-Za-z0-9._-])
    task_name: String,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct ResumeAgentParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Task name (must match [A-Za-z0-9._-])
    task_name: String,
    /// Follow-up prompt sent to `codex exec resume`
    follow_up_prompt: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendMessageParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Conversation thread identifier
    thread_id: String,
    /// Sending agent identifier
    from_agent: String,
    /// Receiving agent identifier
    to_agent: String,
    /// instruction | observation | artifact_ref | critique | decision
    msg_type: String,
    /// Plain-text message body
    body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListMailboxParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Agent id to read messages for
    agent_id: String,
    /// Maximum messages to return (default 50)
    #[serde(default)]
    limit: Option<usize>,
    /// Messages to skip from the start of the sorted mailbox (default 0)
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DispatchTaskParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Task name (must match [A-Za-z0-9._-])
    name: String,
    /// Working directory for the task
    cwd: String,
    /// Prompt passed to the task
    prompt: String,
    /// Sandbox mode: read-only, read-write, or network-read-only
    #[serde(default)]
    sandbox: Option<String>,
    /// Model override for the task
    #[serde(default)]
    model: Option<String>,
    /// Accepted task names this task depends on
    #[serde(default)]
    depends_on: Vec<String>,
    /// Agent identifier recorded in task metadata
    #[serde(default)]
    agent_id: Option<String>,
    /// Conversation thread identifier recorded in task metadata
    #[serde(default)]
    thread_id: Option<String>,
    /// Approval policy passed through to Codex
    #[serde(default)]
    ask_for_approval: Option<String>,
    /// Repeated config overrides passed through to Codex
    #[serde(default)]
    config_overrides: Vec<String>,
    /// Profile override for the task
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DispatchWaveParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Tasks to enqueue together as one dispatch wave
    tasks: Vec<DispatchTaskParams>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFactParams {
    /// Base directory passed to start_run
    run_dir: String,
    /// Shared fact key
    key: String,
    /// Shared fact value
    value: String,
    /// Optional artifact path associated with the fact
    #[serde(default)]
    artifact_path: Option<String>,
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

#[derive(Debug, Serialize)]
struct ResumeAgentResult {
    /// "resume_started"
    state: String,
}

#[derive(Debug, Serialize)]
struct SessionIdResult {
    session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageResult {
    message_id: String,
}

#[derive(Debug, Serialize)]
struct DispatchResult {
    state: String,
    task_count: usize,
}

#[derive(Debug, Serialize)]
struct SealResult {
    state: String,
}

#[derive(Debug, Serialize)]
struct WriteFactResult {
    state: String,
}

// ── Server state ─────────────────────────────────────────────────────────────

struct ActiveRun {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    dispatch_tx: tokio::sync::mpsc::Sender<crate::coordinator::DispatchRequest>,
    accepted: std::sync::Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
    facts_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
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

    #[tool(
        description = "Start parallel Codex tasks from an inline YAML string. Returns immediately — poll get_status for completion."
    )]
    async fn start_run(&self, p: Parameters<StartRunParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;

        // Canonicalize early so registry key is stable regardless of trailing slashes etc.
        let run_dir = canonicalize_or_create(&p.run_dir)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Parse and validate YAML before touching disk or locking.
        let tasks_config = config::TasksConfig::from_str(&p.tasks_yaml)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let waves = tasks_config
            .into_waves()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let run_id = uuid::Uuid::new_v4().to_string();

        let task_count: usize = waves.iter().map(|w| w.len()).sum();
        let wave_count = waves.len();
        let log_dir = run_dir.join("logs");
        let messages_dir = log_dir.join("messages");
        let out_dir = run_dir.join("outputs");

        // Hold the runs lock for the entire preflight to eliminate the TOCTOU race:
        // two concurrent start_run calls for the same run_dir cannot both pass the
        // duplicate check and then clobber each other's disk state.
        let mut runs = self.state.runs.lock().await;

        if let Some(active) = runs.get(&run_dir) {
            if !active.handle.is_finished() {
                return Err(McpError::invalid_params(
                    format!(
                        "run_dir {:?} already has an active run; cancel it first",
                        run_dir
                    ),
                    None,
                ));
            }
            // Finished run entry: clean it up so we can restart.
            runs.remove(&run_dir);
        }

        // Handle clean / reject stale state.
        if p.clean {
            if log_dir.exists() {
                std::fs::remove_dir_all(&log_dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
            if out_dir.exists() {
                std::fs::remove_dir_all(&out_dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        } else {
            let has_stale = [&log_dir, &out_dir].iter().any(|dir| {
                std::fs::read_dir(dir)
                    .map(|d| d.filter_map(|e| e.ok()).count() > 0)
                    .unwrap_or(false)
            });
            if has_stale {
                return Err(McpError::invalid_params(
                    "run_dir contains existing artifacts; pass clean=true to remove them"
                        .to_string(),
                    None,
                ));
            }
        }

        std::fs::create_dir_all(&log_dir)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        std::fs::create_dir_all(&messages_dir)
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
                m.agent_id = task.agent_id.clone().unwrap_or_else(|| task.name.clone());
                m.run_id = run_id.clone();
                m.thread_id = task.thread_id.clone().unwrap_or_else(|| task.name.clone());
                m.status = meta::TaskStatus::Pending;
                m.save(&log_dir)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            }
        }

        let all_tasks: Vec<_> = waves.into_iter().flatten().collect();
        let accepted_names: HashSet<String> = all_tasks.iter().map(|t| t.name.clone()).collect();
        let accepted = Arc::new(Mutex::new(accepted_names));
        let runner =
            TaskRunner::new(&run_dir).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let cancel = CancellationToken::new();
        let (coord, dispatch_tx) =
            crate::coordinator::RunCoordinator::start(&run_dir, runner, cancel.clone());
        let active_dispatch_tx = dispatch_tx.clone();
        let run_dir_bg = run_dir.clone();
        let state = Arc::clone(&self.state);

        let handle = tokio::spawn(async move {
            let all_ok = coord.run(all_tasks).await;
            if let Ok(mut rm) = meta::RunMeta::load(&run_dir_bg) {
                rm.status = if all_ok {
                    meta::RunStatus::Done
                } else {
                    rm.error = collect_first_task_error(&run_dir_bg.join("logs"));
                    meta::RunStatus::Failed
                };
                let _ = rm.save(&run_dir_bg);
            }
            state.runs.lock().await.remove(&run_dir_bg);
        });

        runs.insert(
            run_dir.clone(),
            ActiveRun {
                cancel,
                handle,
                dispatch_tx,
                accepted,
                facts_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            },
        );
        // Drop the lock now that the run is registered.
        drop(runs);

        if !p.dynamic {
            // Static run: seal immediately so coordinator exits when initial tasks finish.
            let _ = active_dispatch_tx.try_send(crate::coordinator::DispatchRequest::Seal);
        }

        ok_json(&StartRunResult {
            run_dir: run_dir.to_string_lossy().into_owned(),
            task_count,
            wave_count,
            poll_interval_ms: 2000,
        })
    }

    #[tool(
        description = "Get status of all tasks in a run. Poll until run_status is 'done' or 'failed'. Sorted by (wave, name)."
    )]
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
            return ok_json(&GetStatusResult {
                run_status: "not_found".into(),
                tasks: vec![],
            });
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

        ok_json(&GetStatusResult {
            run_status: run_status.into(),
            tasks,
        })
    }

    #[tool(
        description = "Read task output file. Supports chunked reading via offset/max_bytes for large files."
    )]
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

    #[tool(
        description = "Resume a task's saved Codex session in the background using a follow-up prompt."
    )]
    async fn resume_agent(
        &self,
        p: Parameters<ResumeAgentParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        validate_task_name(&p.task_name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let run_dir = PathBuf::from(&p.run_dir);
        let meta_path = task_meta_path(&run_dir, &p.task_name);
        let meta = meta::TaskMeta::load(&meta_path).map_err(|e| {
            McpError::invalid_params(
                format!("failed to load task meta for '{}': {}", p.task_name, e),
                None,
            )
        })?;
        if !meta.status.is_terminal() {
            return Err(McpError::invalid_params(
                format!("task '{}' is not in a terminal state", p.task_name),
                None,
            ));
        }
        let session_id = meta.session_id.clone().ok_or_else(|| {
            McpError::invalid_params(
                format!("task '{}' has no saved session_id", p.task_name),
                None,
            )
        })?;
        let runner =
            TaskRunner::new(&run_dir).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let follow_up_prompt = p.follow_up_prompt;

        tokio::spawn(async move {
            let mut meta = meta;
            if let Err(e) = runner
                .resume_task(&mut meta, &session_id, &follow_up_prompt)
                .await
            {
                eprintln!("[{}] warn: resume failed: {}", meta.name, e);
            }
        });

        ok_json(&ResumeAgentResult {
            state: "resume_started".to_string(),
        })
    }

    #[tool(description = "Get the saved Codex session_id for a task, if one has been recorded.")]
    async fn get_session_id(
        &self,
        p: Parameters<TaskRefParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        validate_task_name(&p.task_name)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let meta_path = task_meta_path(&PathBuf::from(&p.run_dir), &p.task_name);
        let meta = meta::TaskMeta::load(&meta_path).map_err(|e| {
            McpError::invalid_params(
                format!("failed to load task meta for '{}': {}", p.task_name, e),
                None,
            )
        })?;
        ok_json(&SessionIdResult {
            session_id: meta.session_id,
        })
    }

    #[tool(
        description = "Append a swarm message to a thread mailbox under logs/messages/{thread_id}.jsonl."
    )]
    async fn send_message(
        &self,
        p: Parameters<SendMessageParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        ensure_non_empty("thread_id", &p.thread_id)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        ensure_non_empty("from_agent", &p.from_agent)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        ensure_non_empty("to_agent", &p.to_agent)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let run_dir = PathBuf::from(&p.run_dir);
        let messages_path = run_dir
            .join("logs")
            .join("messages")
            .join(format!("{}.jsonl", p.thread_id));
        let (run_id, task_id) = resolve_message_context(&run_dir, &p.thread_id, &p.from_agent)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let msg = MessageEnvelope::new(
            run_id,
            task_id,
            p.thread_id,
            p.from_agent,
            p.to_agent,
            parse_message_type(&p.msg_type),
            serde_json::Value::String(p.body),
        );

        message::append_jsonl(&messages_path, &msg)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        ok_json(&SendMessageResult {
            message_id: msg.message_id,
        })
    }

    #[tool(
        description = "List messages addressed to an agent from all thread mailboxes in logs/messages/."
    )]
    async fn list_mailbox(
        &self,
        p: Parameters<ListMailboxParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        ensure_non_empty("agent_id", &p.agent_id)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let messages = list_mailbox_messages(
            &PathBuf::from(&p.run_dir),
            &p.agent_id,
            p.limit.unwrap_or(50),
            p.offset.unwrap_or(0),
        )
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        ok_json(&messages)
    }

    #[tool(description = "Dispatch a single new task into an active run.")]
    async fn dispatch_task(
        &self,
        p: Parameters<DispatchTaskParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let run_dir = p.run_dir.clone();
        let task =
            params_to_task_def(p).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let (dispatch_tx, accepted, cancel) = active_dispatch_state(&self.state, &run_dir).await?;
        if cancel.is_cancelled() {
            return Err(McpError::invalid_params(
                "run has been cancelled; no new tasks accepted",
                None,
            ));
        }

        let mut accepted_guard = accepted.lock().await;
        crate::coordinator::validate_dispatch_name(&task.name, &accepted_guard)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        crate::coordinator::validate_dispatch_deps(&task.depends_on, &accepted_guard)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let task_name = task.name.clone();
        try_send_dispatch(
            &dispatch_tx,
            crate::coordinator::DispatchRequest::Task(task),
            &format!("task '{}'", task_name),
        )?;
        accepted_guard.insert(task_name);

        ok_json(&DispatchResult {
            state: "accepted".to_string(),
            task_count: 1,
        })
    }

    #[tool(description = "Dispatch a batch of new tasks into an active run.")]
    async fn dispatch_wave(
        &self,
        p: Parameters<DispatchWaveParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let run_dir = p.run_dir;
        let tasks = p
            .tasks
            .into_iter()
            .map(params_to_task_def)
            .collect::<Result<Vec<_>>>()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let task_count = tasks.len();
        let (dispatch_tx, accepted, cancel) = active_dispatch_state(&self.state, &run_dir).await?;
        if cancel.is_cancelled() {
            return Err(McpError::invalid_params(
                "run has been cancelled; no new tasks accepted",
                None,
            ));
        }

        let mut accepted_guard = accepted.lock().await;
        let batch_names = validate_dispatch_wave_defs(&tasks, &accepted_guard)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        try_send_dispatch(
            &dispatch_tx,
            crate::coordinator::DispatchRequest::Wave(tasks),
            "task wave",
        )?;
        accepted_guard.extend(batch_names);

        ok_json(&DispatchResult {
            state: "accepted".to_string(),
            task_count,
        })
    }

    #[tool(description = "Seal an active run so no more dispatches should be added.")]
    async fn seal_dispatch(&self, p: Parameters<RunDirParam>) -> Result<CallToolResult, McpError> {
        let (dispatch_tx, _, _) = active_dispatch_state(&self.state, &p.0.run_dir).await?;
        try_send_dispatch(
            &dispatch_tx,
            crate::coordinator::DispatchRequest::Seal,
            "dispatch seal",
        )?;
        ok_json(&SealResult {
            state: "seal_requested".to_string(),
        })
    }

    #[tool(description = "Write a shared fact for a run.")]
    async fn write_fact(&self, p: Parameters<WriteFactParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        ensure_non_empty("key", &p.key)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let run_dir = PathBuf::from(&p.run_dir);
        let run_dir = run_dir.canonicalize().unwrap_or(run_dir);
        let facts_lock = {
            let runs = self.state.runs.lock().await;
            runs.get(&run_dir)
                .map(|active| Arc::clone(&active.facts_lock))
        };
        let _facts_guard = if let Some(facts_lock) = facts_lock {
            Some(facts_lock.lock_owned().await)
        } else {
            None
        };
        let artifact_path = p.artifact_path.map(PathBuf::from);
        FactsStore::new(Path::new(&run_dir))
            .write_fact(&p.key, p.value, artifact_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&WriteFactResult {
            state: "written".to_string(),
        })
    }

    #[tool(description = "Read all shared facts for a run.")]
    async fn read_facts(&self, p: Parameters<RunDirParam>) -> Result<CallToolResult, McpError> {
        let facts = FactsStore::new(Path::new(&p.0.run_dir))
            .read_all()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ok_json(&facts)
    }

    #[tool(
        description = "Cancel an active run. Returns promptly; tasks converge to cancelled state."
    )]
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
                let _ = active
                    .dispatch_tx
                    .try_send(crate::coordinator::DispatchRequest::Seal);
                "cancel_requested"
            }
        } else {
            "not_found"
        };
        ok_json(&CancelResult {
            state: state_str.into(),
        })
    }
}

impl ServerHandler for CodexParServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
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
        async move {
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }
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

fn ensure_non_empty(name: &str, value: &str) -> Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "{name} cannot be empty");
    Ok(())
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
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return None;
    };
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

async fn active_dispatch_state(
    state: &Arc<ServerState>,
    run_dir: &str,
) -> Result<
    (
        tokio::sync::mpsc::Sender<crate::coordinator::DispatchRequest>,
        Arc<Mutex<HashSet<String>>>,
        CancellationToken,
    ),
    McpError,
> {
    let run_dir = PathBuf::from(run_dir);
    let canon = run_dir.canonicalize().unwrap_or(run_dir);
    let mut runs = state.runs.lock().await;
    let Some(active) = runs.get(&canon) else {
        return Err(McpError::invalid_params(
            format!("run_dir {:?} has no active run", canon),
            None,
        ));
    };
    if active.handle.is_finished() {
        runs.remove(&canon);
        return Err(McpError::invalid_params(
            format!("run_dir {:?} is already terminal", canon),
            None,
        ));
    }
    Ok((
        active.dispatch_tx.clone(),
        Arc::clone(&active.accepted),
        active.cancel.clone(),
    ))
}

fn try_send_dispatch(
    dispatch_tx: &tokio::sync::mpsc::Sender<crate::coordinator::DispatchRequest>,
    request: crate::coordinator::DispatchRequest,
    label: &str,
) -> Result<(), McpError> {
    match dispatch_tx.try_send(request) {
        Ok(()) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(McpError::invalid_params(
            format!(
                "{} cannot be accepted because the run is no longer dispatchable",
                label
            ),
            None,
        )),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(McpError::internal_error(
            format!("dispatch queue is full while enqueuing {}", label),
            None,
        )),
    }
}

fn validate_dispatch_wave_defs(
    tasks: &[crate::config::TaskDef],
    accepted: &HashSet<String>,
) -> Result<HashSet<String>> {
    let mut synthetic = accepted
        .iter()
        .cloned()
        .map(dispatch_validation_placeholder)
        .collect::<Vec<_>>();
    synthetic.extend(tasks.iter().cloned());
    config::TasksConfig { tasks: synthetic }.into_waves()?;

    let mut batch_names = HashSet::new();
    for task in tasks {
        crate::coordinator::validate_dispatch_name(&task.name, accepted)?;
        anyhow::ensure!(
            batch_names.insert(task.name.clone()),
            "duplicate task name: '{}'",
            task.name
        );
    }

    Ok(batch_names)
}

fn dispatch_validation_placeholder(name: String) -> crate::config::TaskDef {
    crate::config::TaskDef {
        name,
        cwd: PathBuf::from("."),
        prompt: String::new(),
        sandbox: crate::config::Sandbox::ReadOnly,
        agent_id: None,
        thread_id: None,
        output_schema: None,
        ephemeral: false,
        add_dirs: vec![],
        ask_for_approval: "never".to_string(),
        config_overrides: vec![],
        profile: None,
        model: None,
        depends_on: vec![],
    }
}

fn params_to_task_def(p: DispatchTaskParams) -> anyhow::Result<crate::config::TaskDef> {
    use crate::config::{Sandbox, TaskDef};

    anyhow::ensure!(!p.cwd.trim().is_empty(), "cwd cannot be empty");
    let sandbox = match p.sandbox.as_deref().unwrap_or("read-only") {
        "read-only" => Sandbox::ReadOnly,
        "read-write" => Sandbox::ReadWrite,
        "network-read-only" => Sandbox::NetworkReadOnly,
        other => {
            anyhow::bail!(
                "unknown sandbox '{}'; use read-only, read-write, or network-read-only",
                other
            )
        }
    };
    Ok(TaskDef {
        name: p.name,
        cwd: std::path::PathBuf::from(p.cwd),
        prompt: p.prompt,
        sandbox,
        agent_id: p.agent_id,
        thread_id: p.thread_id,
        output_schema: None,
        ephemeral: false,
        add_dirs: vec![],
        model: p.model,
        depends_on: p.depends_on,
        ask_for_approval: p.ask_for_approval.unwrap_or_else(|| "never".to_string()),
        config_overrides: p.config_overrides,
        profile: p.profile,
    })
}

fn resolve_message_context(
    run_dir: &Path,
    thread_id: &str,
    from_agent: &str,
) -> Result<(String, String)> {
    let metas = read_task_metas(&run_dir.join("logs"))?;

    let selected = metas
        .iter()
        .find(|m| m.agent_id == from_agent && m.thread_id == thread_id)
        .or_else(|| metas.iter().find(|m| m.agent_id == from_agent))
        .or_else(|| metas.iter().find(|m| m.thread_id == thread_id));

    let run_id = selected
        .map(|m| m.run_id.clone())
        .or_else(|| {
            metas
                .iter()
                .find(|m| !m.run_id.is_empty())
                .map(|m| m.run_id.clone())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no task metadata with run_id found in {}",
                run_dir.display()
            )
        })?;
    let task_id = selected
        .map(|m| m.name.clone())
        .unwrap_or_else(|| from_agent.to_string());

    Ok((run_id, task_id))
}

fn list_mailbox_messages(
    run_dir: &Path,
    agent_id: &str,
    limit: usize,
    offset: usize,
) -> Result<Vec<MessageEnvelope>> {
    use std::io::BufRead;

    let messages_dir = run_dir.join("logs").join("messages");
    if !messages_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&messages_dir)?
        .filter_map(|entry| entry.ok())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());

    let mut messages = Vec::new();
    for entry in entries {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        let file = std::fs::File::open(&path)?;
        for (line_idx, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let envelope: MessageEnvelope = serde_json::from_str(&line).with_context(|| {
                format!(
                    "failed to parse message in {} at line {}",
                    path.display(),
                    line_idx + 1
                )
            })?;
            if envelope.to_agent == agent_id {
                messages.push(envelope);
            }
        }
    }

    messages.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(messages.into_iter().skip(offset).take(limit).collect())
}

fn parse_message_type(msg_type: &str) -> MessageType {
    match msg_type.trim().to_ascii_lowercase().as_str() {
        "instruction" => MessageType::Instruction,
        "artifact_ref" | "artifact-ref" => MessageType::ArtifactRef,
        "critique" => MessageType::Critique,
        "decision" => MessageType::Decision,
        "observation" => MessageType::Observation,
        _ => MessageType::Observation,
    }
}

fn read_task_metas(log_dir: &Path) -> Result<Vec<meta::TaskMeta>> {
    let mut entries: Vec<_> = std::fs::read_dir(log_dir)?
        .filter_map(|entry| entry.ok())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());

    let mut metas = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".meta.json") || name.ends_with(".meta.json.tmp") {
            continue;
        }
        metas.push(meta::TaskMeta::load(&entry.path())?);
    }

    Ok(metas)
}

fn task_meta_path(run_dir: &Path, task_name: &str) -> PathBuf {
    run_dir
        .join("logs")
        .join(format!("{}.meta.json", task_name))
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
            assert!(
                validate_task_name(name).is_err(),
                "expected err for {name:?}"
            );
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

    #[test]
    fn params_to_task_def_rejects_empty_cwd() {
        let err = params_to_task_def(DispatchTaskParams {
            run_dir: "/tmp/run".into(),
            name: "task-a".into(),
            cwd: "   ".into(),
            prompt: "do work".into(),
            sandbox: None,
            model: None,
            depends_on: vec![],
            agent_id: None,
            thread_id: None,
            ask_for_approval: None,
            config_overrides: vec![],
            profile: None,
        })
        .unwrap_err();

        assert!(err.to_string().contains("cwd cannot be empty"));
    }

    #[test]
    fn params_to_task_def_rejects_unknown_sandbox() {
        let err = params_to_task_def(DispatchTaskParams {
            run_dir: "/tmp/run".into(),
            name: "task-a".into(),
            cwd: ".".into(),
            prompt: "do work".into(),
            sandbox: Some("nope".into()),
            model: None,
            depends_on: vec![],
            agent_id: None,
            thread_id: None,
            ask_for_approval: None,
            config_overrides: vec![],
            profile: None,
        })
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("unknown sandbox 'nope'; use read-only, read-write, or network-read-only"));
    }

    #[test]
    fn parse_message_type_defaults_to_observation() {
        assert_eq!(parse_message_type("instruction"), MessageType::Instruction);
        assert_eq!(parse_message_type("artifact-ref"), MessageType::ArtifactRef);
        assert_eq!(parse_message_type("unknown"), MessageType::Observation);
    }

    #[test]
    fn resolve_message_context_prefers_exact_agent_and_thread_match() {
        let dir = TempDir::new().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let mut first = meta::TaskMeta::new("planner-task", "/tmp", "plan");
        first.agent_id = "planner".into();
        first.thread_id = "thread-a".into();
        first.run_id = "run-123".into();
        first.save(&log_dir).unwrap();

        let mut second = meta::TaskMeta::new("reviewer-task", "/tmp", "review");
        second.agent_id = "planner".into();
        second.thread_id = "thread-b".into();
        second.run_id = "run-123".into();
        second.save(&log_dir).unwrap();

        let (run_id, task_id) = resolve_message_context(dir.path(), "thread-b", "planner").unwrap();
        assert_eq!(run_id, "run-123");
        assert_eq!(task_id, "reviewer-task");
    }

    #[test]
    fn list_mailbox_messages_filters_sorts_and_pages() {
        let dir = TempDir::new().unwrap();
        let messages_dir = dir.path().join("logs").join("messages");
        std::fs::create_dir_all(&messages_dir).unwrap();

        let mut first = MessageEnvelope::new(
            "run-1",
            "task-1",
            "thread-a",
            "planner",
            "reviewer",
            MessageType::Observation,
            serde_json::Value::String("first".into()),
        );
        let mut second = MessageEnvelope::new(
            "run-1",
            "task-2",
            "thread-b",
            "planner",
            "reviewer",
            MessageType::Decision,
            serde_json::Value::String("second".into()),
        );
        let third = MessageEnvelope::new(
            "run-1",
            "task-3",
            "thread-c",
            "planner",
            "other-agent",
            MessageType::Critique,
            serde_json::Value::String("ignore".into()),
        );

        first.created_at = chrono::Local::now() - chrono::Duration::seconds(10);
        second.created_at = chrono::Local::now();

        message::append_jsonl(&messages_dir.join("thread-a.jsonl"), &second).unwrap();
        message::append_jsonl(&messages_dir.join("thread-b.jsonl"), &first).unwrap();
        message::append_jsonl(&messages_dir.join("thread-c.jsonl"), &third).unwrap();

        let page = list_mailbox_messages(dir.path(), "reviewer", 1, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].body, serde_json::Value::String("second".into()));
    }
}
