/// Behavior/integration tests for `codex-par serve`.
///
/// These tests spawn the MCP server as a subprocess, speak line-delimited
/// JSON-RPC over stdio, and assert the agreed tool behavior end-to-end.
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    mpsc::{self, Receiver, RecvTimeoutError},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tempfile::TempDir;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const RUN_TIMEOUT: Duration = Duration::from_secs(5);
const SLOW_RUN_TIMEOUT: Duration = Duration::from_secs(10);

fn fake_codex_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_fake_codex") {
        return PathBuf::from(p);
    }

    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("debug");
    p.push("fake_codex");
    p
}

fn codex_par_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_codex-par") {
        return PathBuf::from(p);
    }

    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("debug");
    p.push("codex-par");
    p
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct StartRunResult {
    run_dir: String,
    task_count: u64,
    wave_count: u64,
    poll_interval_ms: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct StatusResult {
    run_status: String,
    tasks: Vec<TaskStatusRow>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct TaskStatusRow {
    name: String,
    status: String,
    wave: u64,
    tokens_in: u64,
    tokens_out: u64,
    last_action: String,
    error: Option<String>,
    exit_code: Option<i32>,
    has_output: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ReadChunk {
    content: String,
    next_offset: u64,
    eof: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct CancelRunResult {
    state: String,
}

#[allow(dead_code)]
#[derive(Debug)]
enum StdoutEvent {
    Json(Value),
    InvalidJson(String),
    ReadError(String),
}

#[allow(dead_code)]
struct McpServer {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout_rx: Receiver<StdoutEvent>,
    backlog: Vec<Value>,
    notifications: Vec<Value>,
    stderr_buffer: Arc<Mutex<String>>,
    next_id: u64,
}

#[allow(dead_code)]
impl McpServer {
    fn spawn_with_codex_bin(workdir: &Path, codex_bin: &Path) -> Self {
        let codex_par = codex_par_bin();
        assert!(
            codex_par.exists(),
            "codex-par binary not found at {}. Run `cargo build` first.",
            codex_par.display()
        );

        let mut child = Command::new(&codex_par)
            .arg("serve")
            .current_dir(workdir)
            .env("CODEX_BIN", codex_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn `codex-par serve`");

        let stdin = BufWriter::new(child.stdin.take().expect("stdin should be piped"));
        let stdout = child.stdout.take().expect("stdout should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        let (stdout_tx, stdout_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let event = match line {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(message) => StdoutEvent::Json(message),
                        Err(err) => StdoutEvent::InvalidJson(format!(
                            "non-JSON stdout line: {:?} ({})",
                            line, err
                        )),
                    },
                    Err(err) => StdoutEvent::ReadError(format!("failed reading stdout: {}", err)),
                };

                if stdout_tx.send(event).is_err() {
                    break;
                }
            }
        });

        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buffer_clone = Arc::clone(&stderr_buffer);
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let mut buf = stderr_buffer_clone.lock().unwrap();
                match line {
                    Ok(line) => {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                    Err(err) => {
                        buf.push_str(&format!("[stderr read error] {}\n", err));
                        break;
                    }
                }
            }
        });

        Self {
            child,
            stdin,
            stdout_rx,
            backlog: Vec::new(),
            notifications: Vec::new(),
            stderr_buffer,
            next_id: 1,
        }
    }

    fn stderr_dump(&self) -> String {
        let stderr = self.stderr_buffer.lock().unwrap().clone();
        if stderr.is_empty() {
            "<empty>".to_string()
        } else {
            stderr
        }
    }

    fn send_message(&mut self, message: &Value) {
        serde_json::to_writer(&mut self.stdin, message).expect("serialize JSON-RPC message");
        self.stdin
            .write_all(b"\n")
            .expect("write JSON-RPC newline delimiter");
        self.stdin.flush().expect("flush JSON-RPC request");
    }

    fn take_backlog_response(&mut self, id: u64) -> Option<Value> {
        let wanted = json!(id);
        let pos = self
            .backlog
            .iter()
            .position(|message| message.get("id") == Some(&wanted))?;
        Some(self.backlog.swap_remove(pos))
    }

    fn wait_for_response(&mut self, id: u64, timeout: Duration) -> Value {
        if let Some(message) = self.take_backlog_response(id) {
            return message;
        }

        let deadline = Instant::now() + timeout;
        let wanted = json!(id);

        loop {
            let now = Instant::now();
            if now >= deadline {
                panic!(
                    "timed out waiting for JSON-RPC response id={}. stderr:\n{}",
                    id,
                    self.stderr_dump()
                );
            }

            let remaining = deadline - now;
            match self.stdout_rx.recv_timeout(remaining) {
                Ok(StdoutEvent::Json(message)) => {
                    if message.get("id") == Some(&wanted) {
                        return message;
                    }

                    if message.get("id").is_some() {
                        self.backlog.push(message);
                    } else {
                        self.notifications.push(message);
                    }
                }
                Ok(StdoutEvent::InvalidJson(err)) => {
                    panic!("server wrote stray stdout: {}\nstderr:\n{}", err, self.stderr_dump());
                }
                Ok(StdoutEvent::ReadError(err)) => {
                    panic!("failed reading server stdout: {}\nstderr:\n{}", err, self.stderr_dump());
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "timed out waiting for JSON-RPC response id={}. stderr:\n{}",
                        id,
                        self.stderr_dump()
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let exit = self.child.try_wait().ok().flatten();
                    panic!(
                        "server stdout closed while waiting for id={}. exit={:?}\nstderr:\n{}",
                        id,
                        exit,
                        self.stderr_dump()
                    );
                }
            }
        }
    }

    fn request_raw(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        self.send_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        self.wait_for_response(id, timeout)
    }

    fn request_ok(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let response = self.request_raw(method, params, timeout);
        if let Some(error) = response.get("error") {
            panic!(
                "{} returned JSON-RPC error: {}\nstderr:\n{}",
                method,
                error,
                self.stderr_dump()
            );
        }
        response
    }

    fn initialize(&mut self) -> Value {
        // Use the current stable MCP revision and accept the server's negotiated response version.
        let response = self.request_ok(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "codex-par-tests",
                    "version": "1.0.0",
                }
            }),
            REQUEST_TIMEOUT,
        );

        assert!(
            response
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
                .is_some(),
            "initialize response missing protocolVersion: {}",
            response
        );

        self.send_message(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));

        response
    }

    fn tools_list(&mut self) -> Vec<String> {
        let response = self.request_ok("tools/list", json!({}), REQUEST_TIMEOUT);
        let tools = response
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools/list response missing result.tools");

        tools
            .iter()
            .map(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .expect("tool entry missing name")
                    .to_string()
            })
            .collect()
    }

    fn call_tool_raw(&mut self, name: &str, arguments: Value, timeout: Duration) -> Value {
        self.request_raw(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
            timeout,
        )
    }

    fn call_tool_ok<T: DeserializeOwned>(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> T {
        let response = self.call_tool_raw(name, arguments, timeout);

        if let Some(error) = response.get("error") {
            panic!(
                "tool {} returned JSON-RPC error: {}\nstderr:\n{}",
                name,
                error,
                self.stderr_dump()
            );
        }

        let result = response
            .get("result")
            .unwrap_or_else(|| panic!("tool {} response missing result: {}", name, response));

        assert!(
            !tool_result_is_error(result),
            "tool {} returned tool error: {}\nstderr:\n{}",
            name,
            tool_result_message(result),
            self.stderr_dump()
        );

        let payload = extract_tool_payload(result);
        serde_json::from_value(payload).unwrap_or_else(|err| {
            panic!(
                "failed to parse tool {} result: {}\nresponse: {}\nstderr:\n{}",
                name,
                err,
                response,
                self.stderr_dump()
            )
        })
    }

    fn call_tool_jsonrpc_error(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> (i64, String) {
        let response = self.call_tool_raw(name, arguments, timeout);
        let error = response
            .get("error")
            .unwrap_or_else(|| panic!("expected JSON-RPC error response, got {}", response));

        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .expect("JSON-RPC error missing numeric code");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        (code, message)
    }

    fn call_tool_rejection_message(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> String {
        let response = self.call_tool_raw(name, arguments, timeout);

        if let Some(error) = response.get("error") {
            return error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }

        let result = response
            .get("result")
            .unwrap_or_else(|| panic!("expected rejection response, got {}", response));

        if tool_result_is_error(result) {
            return tool_result_message(result);
        }

        panic!("expected tool rejection, got success: {}", response);
    }

    fn start_run(&mut self, tasks_yaml: &str, run_dir: &Path, clean: bool) -> StartRunResult {
        self.call_tool_ok(
            "start_run",
            json!({
                "tasks_yaml": tasks_yaml,
                "run_dir": run_dir_string(run_dir),
                "clean": clean,
            }),
            REQUEST_TIMEOUT,
        )
    }

    fn get_status(&mut self, run_dir: &Path) -> StatusResult {
        self.call_tool_ok(
            "get_status",
            json!({
                "run_dir": run_dir_string(run_dir),
            }),
            REQUEST_TIMEOUT,
        )
    }

    fn read_stream(
        &mut self,
        tool_name: &str,
        run_dir: &Path,
        task_name: &str,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> ReadChunk {
        let mut args = serde_json::Map::new();
        args.insert("run_dir".to_string(), json!(run_dir_string(run_dir)));
        args.insert("task_name".to_string(), json!(task_name));
        if let Some(offset) = offset {
            args.insert("offset".to_string(), json!(offset));
        }
        if let Some(max_bytes) = max_bytes {
            args.insert("max_bytes".to_string(), json!(max_bytes));
        }

        self.call_tool_ok(tool_name, Value::Object(args), REQUEST_TIMEOUT)
    }

    fn read_output(
        &mut self,
        run_dir: &Path,
        task_name: &str,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> ReadChunk {
        self.read_stream("read_output", run_dir, task_name, offset, max_bytes)
    }

    fn read_stderr(
        &mut self,
        run_dir: &Path,
        task_name: &str,
        offset: Option<u64>,
        max_bytes: Option<u64>,
    ) -> ReadChunk {
        self.read_stream("read_stderr", run_dir, task_name, offset, max_bytes)
    }

    fn cancel_run(&mut self, run_dir: &Path) -> CancelRunResult {
        self.call_tool_ok(
            "cancel_run",
            json!({
                "run_dir": run_dir_string(run_dir),
            }),
            REQUEST_TIMEOUT,
        )
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.stdin.flush();

        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

#[allow(dead_code)]
fn tool_result_is_error(result: &Value) -> bool {
    result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[allow(dead_code)]
fn tool_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[allow(dead_code)]
fn extract_tool_payload(result: &Value) -> Value {
    if let Some(structured) = result.get("structuredContent") {
        return structured.clone();
    }

    let text = tool_text(result);
    if !text.is_empty() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            return parsed;
        }
        return json!({ "message": text });
    }

    result.clone()
}

#[allow(dead_code)]
fn tool_result_message(result: &Value) -> String {
    let payload = extract_tool_payload(result);
    if let Some(message) = payload.get("message").and_then(Value::as_str) {
        return message.to_string();
    }

    let text = tool_text(result);
    if !text.is_empty() {
        return text;
    }

    payload.to_string()
}

#[allow(dead_code)]
fn spawn_initialized_server(workdir: &Path) -> McpServer {
    let fake = fake_codex_bin();
    assert!(
        fake.exists(),
        "fake_codex binary not found at {}. Run `cargo build` first.",
        fake.display()
    );

    let mut server = McpServer::spawn_with_codex_bin(workdir, &fake);
    server.initialize();
    server
}

#[allow(dead_code)]
fn spawn_initialized_server_with_codex_bin(workdir: &Path, codex_bin: &Path) -> McpServer {
    let mut server = McpServer::spawn_with_codex_bin(workdir, codex_bin);
    server.initialize();
    server
}

#[allow(dead_code)]
fn run_dir_string(path: &Path) -> String {
    path.to_str()
        .expect("temp paths used by tests should be valid UTF-8")
        .to_string()
}

#[allow(dead_code)]
fn load_task_meta(run_dir: &Path, task_name: &str) -> Value {
    let path = run_dir.join("logs").join(format!("{}.meta.json", task_name));
    let content = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "task meta not found for '{}' at {}",
            task_name,
            path.display()
        )
    });
    serde_json::from_str(&content).expect("parse task meta json")
}

#[allow(dead_code)]
fn load_run_meta(run_dir: &Path) -> Value {
    let candidates = [run_dir.join("run.meta.json"), run_dir.join("logs").join("run.meta.json")];

    for path in candidates {
        if path.exists() {
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("failed reading {}", path.display()));
            return serde_json::from_str(&content).expect("parse run meta json");
        }
    }

    panic!(
        "run.meta.json not found under {}",
        run_dir.display()
    );
}

#[allow(dead_code)]
fn task<'a>(status: &'a StatusResult, name: &str) -> &'a TaskStatusRow {
    status.tasks.iter().find(|t| t.name == name).unwrap_or_else(|| {
        panic!(
            "task '{}' not found in status payload with tasks {:?}",
            name,
            status
                .tasks
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
        )
    })
}

#[allow(dead_code)]
fn wait_for_status<F>(
    server: &mut McpServer,
    run_dir: &Path,
    poll_interval_ms: u64,
    timeout: Duration,
    predicate: F,
) -> StatusResult
where
    F: Fn(&StatusResult) -> bool,
{
    let started = Instant::now();
    let sleep_for = Duration::from_millis(poll_interval_ms.clamp(10, 100));

    let mut last = server.get_status(run_dir);
    if predicate(&last) {
        return last;
    }

    while started.elapsed() < timeout {
        std::thread::sleep(sleep_for);
        last = server.get_status(run_dir);
        if predicate(&last) {
            return last;
        }
    }

    panic!(
        "timed out waiting for status on {}. last status: {:?}\nstderr:\n{}",
        run_dir.display(),
        last,
        server.stderr_dump()
    );
}

#[allow(dead_code)]
fn wait_for_run_to_stop(
    server: &mut McpServer,
    run_dir: &Path,
    poll_interval_ms: u64,
    timeout: Duration,
) -> StatusResult {
    wait_for_status(server, run_dir, poll_interval_ms, timeout, |status| {
        status.run_status != "running"
    })
}

// ---------------------------------------------------------------------------
// Test 1: initialize + tools/list returns expected tool names, no stray stdout
// ---------------------------------------------------------------------------

#[test]
fn initialize_and_tools_list_return_expected_tool_names() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());

    // Any stray non-protocol stdout would already fail in the helper reader thread.
    let names: BTreeSet<_> = server.tools_list().into_iter().collect();
    let expected: BTreeSet<_> = [
        "cancel_run",
        "get_status",
        "read_output",
        "read_stderr",
        "start_run",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    assert_eq!(names, expected);
}

// ---------------------------------------------------------------------------
// Test 2: malformed start_run params -> invalid-params, server stays usable
// ---------------------------------------------------------------------------

#[test]
fn malformed_start_run_params_return_invalid_params_and_server_stays_usable() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());

    let (code, _message) = server.call_tool_jsonrpc_error(
        "start_run",
        json!({
            "tasks_yaml": 17,
            "run_dir": true,
            "clean": "nope",
        }),
        REQUEST_TIMEOUT,
    );

    assert_eq!(code, -32602);

    let names: BTreeSet<_> = server.tools_list().into_iter().collect();
    assert!(names.contains("start_run"));
}

// ---------------------------------------------------------------------------
// Test 3: valid YAML slow task: start_run returns before task completes
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow behavior test"]
fn valid_yaml_slow_task_start_run_returns_before_task_completes() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "slow"
    cwd: "/tmp"
    prompt: "Please SLOW_SUCCEED"
"#;

    let start_call = Instant::now();
    let started = server.start_run(yaml, &run_dir, false);
    let elapsed = start_call.elapsed();

    assert_eq!(PathBuf::from(&started.run_dir), run_dir);
    assert_eq!(started.task_count, 1);
    assert_eq!(started.wave_count, 1);
    assert!(started.poll_interval_ms > 0);

    let status = server.get_status(&run_dir);
    assert_eq!(status.run_status, "running");
    assert!(matches!(task(&status, "slow").status.as_str(), "pending" | "running"));
    assert!(
        elapsed < Duration::from_millis(500),
        "start_run blocked too long: {:?}",
        elapsed
    );

    let terminal =
        wait_for_run_to_stop(&mut server, &run_dir, started.poll_interval_ms, SLOW_RUN_TIMEOUT);
    assert_eq!(terminal.run_status, "done");
    assert_eq!(task(&terminal, "slow").status, "done");
}

// ---------------------------------------------------------------------------
// Test 4: start_run creates all pending task metas before returning
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow behavior test"]
fn start_run_creates_all_pending_task_metas_before_returning() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "root"
    cwd: "/tmp"
    prompt: "Please HANG"
  - name: "sibling"
    cwd: "/tmp"
    prompt: "Please HANG"
  - name: "downstream"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["root"]
"#;

    let started = server.start_run(yaml, &run_dir, false);
    assert_eq!(started.task_count, 3);
    assert_eq!(started.wave_count, 2);

    let root_meta = load_task_meta(&run_dir, "root");
    let sibling_meta = load_task_meta(&run_dir, "sibling");
    let downstream_meta = load_task_meta(&run_dir, "downstream");

    assert_eq!(root_meta["wave"].as_u64(), Some(0));
    assert_eq!(sibling_meta["wave"].as_u64(), Some(0));
    assert_eq!(downstream_meta["wave"].as_u64(), Some(1));
    assert_eq!(downstream_meta["status"].as_str(), Some("pending"));

    let cancel = server.cancel_run(&run_dir);
    assert_eq!(cancel.state, "cancel_requested");

    let terminal = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| !status.tasks.is_empty() && status.tasks.iter().all(|t| t.status == "cancelled"),
    );
    assert!(terminal.tasks.iter().all(|t| t.status == "cancelled"));
}

// ---------------------------------------------------------------------------
// Test 5: multi-wave: wave 1 tasks stay pending while wave 0 runs
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow behavior test"]
fn multi_wave_downstream_tasks_stay_pending_while_wave0_runs() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "root"
    cwd: "/tmp"
    prompt: "Please HANG"
  - name: "child"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["root"]
"#;

    let started = server.start_run(yaml, &run_dir, false);
    let status = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| {
            status.run_status == "running"
                && status.tasks.iter().any(|t| t.name == "root" && t.status == "running")
                && status.tasks.iter().any(|t| t.name == "child" && t.status == "pending")
        },
    );

    assert_eq!(task(&status, "root").status, "running");
    assert_eq!(task(&status, "child").status, "pending");

    let cancel = server.cancel_run(&run_dir);
    assert_eq!(cancel.state, "cancel_requested");

    let terminal = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| !status.tasks.is_empty() && status.tasks.iter().all(|t| t.status == "cancelled"),
    );
    assert!(terminal.tasks.iter().all(|t| t.status == "cancelled"));
}

// ---------------------------------------------------------------------------
// Test 6: failed wave-0 -> downstream tasks become cancelled, never run
// ---------------------------------------------------------------------------

#[test]
fn failed_wave0_cancels_downstream_tasks_without_running_them() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "root"
    cwd: "/tmp"
    prompt: "Please FAIL"
  - name: "child"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["root"]
"#;

    let started = server.start_run(yaml, &run_dir, false);
    let terminal =
        wait_for_run_to_stop(&mut server, &run_dir, started.poll_interval_ms, RUN_TIMEOUT);

    assert_eq!(terminal.run_status, "failed");

    let root = task(&terminal, "root");
    let child = task(&terminal, "child");

    assert_eq!(root.status, "failed");
    assert_eq!(child.status, "cancelled");
    assert_eq!(child.tokens_in, 0);
    assert_eq!(child.tokens_out, 0);
    assert_eq!(child.exit_code, None);
    assert!(!child.has_output);
    assert!(child.error.is_some());
}

// ---------------------------------------------------------------------------
// Test 7: successful task: read_output returns content, eof=true
// ---------------------------------------------------------------------------

#[test]
fn successful_task_read_output_returns_content_and_eof() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "ok"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
"#;

    let started = server.start_run(yaml, &run_dir, false);
    let terminal =
        wait_for_run_to_stop(&mut server, &run_dir, started.poll_interval_ms, RUN_TIMEOUT);

    assert_eq!(terminal.run_status, "done");
    assert_eq!(task(&terminal, "ok").status, "done");
    assert!(task(&terminal, "ok").has_output);

    let expected = fs::read_to_string(run_dir.join("outputs").join("ok.md"))
        .expect("expected output file for successful task");
    let chunk = server.read_output(&run_dir, "ok", None, None);

    assert_eq!(chunk.content, expected);
    assert_eq!(chunk.next_offset, expected.len() as u64);
    assert!(chunk.eof);
}

// ---------------------------------------------------------------------------
// Test 8: large output: read_output with offset/max_bytes is bounded
// ---------------------------------------------------------------------------

#[test]
fn large_output_read_output_honors_offset_and_max_bytes() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "big"
    cwd: "/tmp"
    prompt: "Please LARGE_OUTPUT"
"#;

    let started = server.start_run(yaml, &run_dir, false);
    let terminal =
        wait_for_run_to_stop(&mut server, &run_dir, started.poll_interval_ms, RUN_TIMEOUT);

    assert_eq!(terminal.run_status, "done");
    assert_eq!(task(&terminal, "big").status, "done");
    assert!(task(&terminal, "big").has_output);

    let expected = fs::read(run_dir.join("outputs").join("big.md"))
        .expect("expected large output file");
    assert!(expected.len() > 256, "fixture output should be meaningfully large");

    let first = server.read_output(&run_dir, "big", Some(0), Some(128));
    assert!(first.content.len() <= 128);
    assert_eq!(first.next_offset, first.content.len() as u64);
    assert!(!first.eof);
    assert_eq!(first.content.as_bytes(), &expected[..first.content.len()]);

    let second = server.read_output(&run_dir, "big", Some(first.next_offset), Some(128));
    assert!(second.content.len() <= 128);
    assert_eq!(
        second.next_offset,
        first.next_offset + second.content.len() as u64
    );
    assert_eq!(
        second.content.as_bytes(),
        &expected[first.next_offset as usize..second.next_offset as usize]
    );
}

// ---------------------------------------------------------------------------
// Test 9: failed task with stderr: read_stderr returns content
// ---------------------------------------------------------------------------

#[test]
fn failed_task_with_stderr_read_stderr_returns_content() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "bad"
    cwd: "/tmp"
    prompt: "Please STDERR_FAIL"
"#;

    let started = server.start_run(yaml, &run_dir, false);
    let terminal =
        wait_for_run_to_stop(&mut server, &run_dir, started.poll_interval_ms, RUN_TIMEOUT);

    assert_eq!(terminal.run_status, "failed");
    assert_eq!(task(&terminal, "bad").status, "failed");
    assert!(!task(&terminal, "bad").has_output);

    let expected = fs::read_to_string(run_dir.join("logs").join("bad.stderr.log"))
        .expect("expected stderr log for failed task");
    let chunk = server.read_stderr(&run_dir, "bad", None, None);

    assert_eq!(chunk.content, expected);
    assert_eq!(chunk.next_offset, expected.len() as u64);
    assert!(chunk.eof);
}

// ---------------------------------------------------------------------------
// Test 10: cancel_run returns promptly, tasks converge to cancelled
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow behavior test"]
fn cancel_run_returns_promptly_and_tasks_converge_to_cancelled() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "root"
    cwd: "/tmp"
    prompt: "Please HANG"
  - name: "child"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["root"]
"#;

    let started = server.start_run(yaml, &run_dir, false);

    let _ = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| {
            status.run_status == "running"
                && status.tasks.iter().any(|t| t.status == "running" || t.status == "pending")
        },
    );

    let cancel_started = Instant::now();
    let cancel = server.cancel_run(&run_dir);
    let elapsed = cancel_started.elapsed();

    assert_eq!(cancel.state, "cancel_requested");
    assert!(
        elapsed < Duration::from_millis(500),
        "cancel_run should return promptly, took {:?}",
        elapsed
    );

    let terminal = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| !status.tasks.is_empty() && status.tasks.iter().all(|t| t.status == "cancelled"),
    );
    assert!(terminal.tasks.iter().all(|t| t.status == "cancelled"));
}

// ---------------------------------------------------------------------------
// Test 11: duplicate start_run for active run_dir -> rejected
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow behavior test"]
fn duplicate_start_run_for_active_run_dir_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "root"
    cwd: "/tmp"
    prompt: "Please HANG"
"#;

    let started = server.start_run(yaml, &run_dir, false);

    let _ = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| status.run_status == "running" && status.tasks.iter().any(|t| t.status == "running"),
    );

    let message = server.call_tool_rejection_message(
        "start_run",
        json!({
            "tasks_yaml": yaml,
            "run_dir": run_dir_string(&run_dir),
            "clean": false,
        }),
        REQUEST_TIMEOUT,
    );
    let lower = message.to_lowercase();

    assert!(
        ["active", "already", "running", "in progress"]
            .iter()
            .any(|needle| lower.contains(needle)),
        "expected duplicate-active-run rejection message, got: {}",
        message
    );

    let status = server.get_status(&run_dir);
    assert_eq!(status.run_status, "running");

    let cancel = server.cancel_run(&run_dir);
    assert_eq!(cancel.state, "cancel_requested");
}

// ---------------------------------------------------------------------------
// Test 12: unknown run_dir for get_status -> clear not_found
// ---------------------------------------------------------------------------

#[test]
fn unknown_run_dir_get_status_returns_not_found() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());

    let missing = tmp.path().join("does-not-exist");
    let status = server.get_status(&missing);

    assert_eq!(status.run_status, "not_found");
    assert!(status.tasks.is_empty());
}

// ---------------------------------------------------------------------------
// Test 13: get_status while running -> prompt + sorted by (wave, name)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "slow behavior test"]
fn get_status_while_running_returns_promptly_and_sorted_by_wave_then_name() {
    let tmp = TempDir::new().unwrap();
    let mut server = spawn_initialized_server(tmp.path());
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "zeta"
    cwd: "/tmp"
    prompt: "Please HANG"
  - name: "alpha"
    cwd: "/tmp"
    prompt: "Please HANG"
  - name: "mid"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["zeta"]
  - name: "beta"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["zeta"]
"#;

    let started = server.start_run(yaml, &run_dir, false);

    let _ = wait_for_status(
        &mut server,
        &run_dir,
        started.poll_interval_ms,
        SLOW_RUN_TIMEOUT,
        |status| status.run_status == "running" && status.tasks.len() == 4,
    );

    let call_started = Instant::now();
    let status = server.get_status(&run_dir);
    let elapsed = call_started.elapsed();

    assert_eq!(status.run_status, "running");
    assert!(
        elapsed < Duration::from_millis(250),
        "get_status should return promptly while running, took {:?}",
        elapsed
    );

    let names: Vec<_> = status.tasks.iter().map(|t| t.name.as_str()).collect();
    let waves: Vec<_> = status.tasks.iter().map(|t| t.wave).collect();

    assert_eq!(names, vec!["alpha", "zeta", "beta", "mid"]);
    assert_eq!(waves, vec![0, 0, 1, 1]);

    let cancel = server.cancel_run(&run_dir);
    assert_eq!(cancel.state, "cancel_requested");
}

// ---------------------------------------------------------------------------
// Test 14: fatal pre-task error -> visible in run.meta.json via get_status
// ---------------------------------------------------------------------------

#[test]
fn fatal_pre_task_error_is_visible_in_run_meta_json_via_get_status() {
    let tmp = TempDir::new().unwrap();
    let missing_codex = tmp.path().join("missing-codex");
    let mut server = spawn_initialized_server_with_codex_bin(tmp.path(), &missing_codex);
    let run_dir = tmp.path().join("run");

    let yaml = r#"
tasks:
  - name: "broken"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
"#;

    let started = server.start_run(yaml, &run_dir, false);
    assert_eq!(started.task_count, 1);

    let terminal =
        wait_for_run_to_stop(&mut server, &run_dir, started.poll_interval_ms, RUN_TIMEOUT);

    assert_eq!(terminal.run_status, "failed");

    let broken = task(&terminal, "broken");
    assert_eq!(broken.status, "failed");
    assert_eq!(broken.exit_code, None);
    assert!(!broken.has_output);

    let error = broken.error.clone().unwrap_or_default();
    assert!(
        error.contains("failed to spawn codex process"),
        "expected spawn failure in task error, got: {}",
        error
    );

    let run_meta = load_run_meta(&run_dir);
    let run_meta_text = run_meta.to_string();
    assert!(
        run_meta_text.contains("failed to spawn codex process"),
        "expected run.meta.json to record the fatal pre-task error, got: {}",
        run_meta_text
    );
}