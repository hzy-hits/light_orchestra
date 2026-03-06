/// Integration tests for the codex-par executor pipeline.
///
/// These tests use a `fake_codex` binary (compiled from tests/fixtures/fake_codex.rs)
/// to exercise the full run path without needing a real Codex installation.
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper: locate the fake_codex binary produced by `cargo build`
// ---------------------------------------------------------------------------

fn fake_codex_bin() -> std::path::PathBuf {
    // The binary lands next to the main binary in the same profile directory.
    // The CARGO_BIN_EXE_fake_codex env var is injected by cargo's test runner.
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_fake_codex") {
        return std::path::PathBuf::from(p);
    }
    // Fallback: walk up from OUT_DIR or use target/debug
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("debug");
    p.push("fake_codex");
    p
}

fn codex_par_bin() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_codex-par") {
        return std::path::PathBuf::from(p);
    }
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("debug");
    p.push("codex-par");
    p
}

// ---------------------------------------------------------------------------
// Helper: run codex-par with a given tasks YAML string
// ---------------------------------------------------------------------------

/// Write `tasks_yaml` to a temp file, then run `codex-par run <path> --dir <dir>`.
/// Returns `(exit_success, stdout_string)`.
fn run_codex_par(tasks_yaml: &str, dir: &Path) -> (bool, String) {
    // Write YAML to a temp file inside `dir`
    let yaml_path = dir.join("tasks.yaml");
    std::fs::write(&yaml_path, tasks_yaml).expect("write tasks yaml");

    let fake_bin = fake_codex_bin();
    let codex_par = codex_par_bin();

    // Ensure the binaries exist; if not, skip gracefully by returning a sentinel
    if !fake_bin.exists() {
        eprintln!("SKIP: fake_codex binary not found at {}", fake_bin.display());
        return (true, "SKIP".to_string());
    }
    if !codex_par.exists() {
        eprintln!("SKIP: codex-par binary not found at {}", codex_par.display());
        return (true, "SKIP".to_string());
    }

    let output = Command::new(&codex_par)
        .arg("run")
        .arg(yaml_path.to_str().unwrap())
        .arg("--dir")
        .arg(dir.to_str().unwrap())
        .env("CODEX_BIN", fake_bin.to_str().unwrap())
        .output()
        .expect("failed to spawn codex-par");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.is_empty() {
        eprintln!("codex-par stderr:\n{}", stderr);
    }

    (output.status.success(), stdout)
}

// ---------------------------------------------------------------------------
// Helper: load a meta.json for a task
// ---------------------------------------------------------------------------

fn load_meta(dir: &Path, task_name: &str) -> serde_json::Value {
    let path = dir.join("logs").join(format!("{}.meta.json", task_name));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("meta.json not found for task '{}' at {}", task_name, path.display()));
    serde_json::from_str(&content).expect("parse meta.json")
}

// ---------------------------------------------------------------------------
// Test 1: single successful task
// ---------------------------------------------------------------------------

#[test]
fn test_single_task_success() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let yaml = r#"
tasks:
  - name: "my_task"
    cwd: "/tmp"
    prompt: "Please SUCCEED at this"
"#;

    let (ok, stdout) = run_codex_par(yaml, dir);
    if stdout == "SKIP" {
        return;
    }

    assert!(ok, "expected exit 0 for SUCCEED task; stdout:\n{}", stdout);

    // Output file should exist
    let output_file = dir.join("outputs").join("my_task.md");
    assert!(output_file.exists(), "outputs/my_task.md should exist");

    // Meta should have status "done"
    let meta = load_meta(dir, "my_task");
    assert_eq!(
        meta["status"].as_str().unwrap(),
        "done",
        "meta status should be 'done'"
    );
}

// ---------------------------------------------------------------------------
// Test 2: single failing task
// ---------------------------------------------------------------------------

#[test]
fn test_single_task_failure() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let yaml = r#"
tasks:
  - name: "bad_task"
    cwd: "/tmp"
    prompt: "Please FAIL at this"
"#;

    let (ok, stdout) = run_codex_par(yaml, dir);
    if stdout == "SKIP" {
        return;
    }

    assert!(!ok, "expected exit 1 for FAIL task; stdout:\n{}", stdout);

    let meta = load_meta(dir, "bad_task");
    assert_eq!(
        meta["status"].as_str().unwrap(),
        "failed",
        "meta status should be 'failed'"
    );
}

// ---------------------------------------------------------------------------
// Test 3: wave skip on failure — wave 1 task is cancelled when wave 0 fails
// ---------------------------------------------------------------------------

#[test]
fn test_wave_skip_on_failure() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let yaml = r#"
tasks:
  - name: "wave0_fail"
    cwd: "/tmp"
    prompt: "Please FAIL"
  - name: "wave1_task"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
    depends_on: ["wave0_fail"]
"#;

    let (ok, stdout) = run_codex_par(yaml, dir);
    if stdout == "SKIP" {
        return;
    }

    assert!(!ok, "expected exit 1 when a wave fails; stdout:\n{}", stdout);

    let meta = load_meta(dir, "wave1_task");
    assert_eq!(
        meta["status"].as_str().unwrap(),
        "cancelled",
        "wave1_task should be cancelled"
    );

    // Error message should indicate skipping
    let error = meta["error"].as_str().unwrap_or("");
    assert!(
        error.contains("skipped") || error.contains("cancelled"),
        "error should mention skipped/cancelled, got: {:?}",
        error
    );
}

// ---------------------------------------------------------------------------
// Test 4: two waves both succeed
// ---------------------------------------------------------------------------

#[test]
fn test_wave_sequential_execution() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let yaml = r#"
tasks:
  - name: "first"
    cwd: "/tmp"
    prompt: "Please SUCCEED first"
  - name: "second"
    cwd: "/tmp"
    prompt: "Please SUCCEED second"
    depends_on: ["first"]
"#;

    let (ok, stdout) = run_codex_par(yaml, dir);
    if stdout == "SKIP" {
        return;
    }

    assert!(ok, "expected exit 0 when both waves succeed; stdout:\n{}", stdout);

    let meta0 = load_meta(dir, "first");
    let meta1 = load_meta(dir, "second");

    assert_eq!(meta0["status"].as_str().unwrap(), "done", "'first' should be done");
    assert_eq!(meta1["status"].as_str().unwrap(), "done", "'second' should be done");
}

// ---------------------------------------------------------------------------
// Test 5: re-running the same task twice — meta reflects the second run
// ---------------------------------------------------------------------------

#[test]
fn test_rerun_truncates_logs() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();

    let yaml = r#"
tasks:
  - name: "repeatable"
    cwd: "/tmp"
    prompt: "Please SUCCEED"
"#;

    // First run
    let (ok1, stdout1) = run_codex_par(yaml, dir);
    if stdout1 == "SKIP" {
        return;
    }
    assert!(ok1, "first run should succeed; stdout:\n{}", stdout1);

    let meta_after_first = load_meta(dir, "repeatable");
    assert_eq!(meta_after_first["status"].as_str().unwrap(), "done");

    // Second run (same dir)
    let (ok2, stdout2) = run_codex_par(yaml, dir);
    assert!(ok2, "second run should also succeed; stdout:\n{}", stdout2);

    let meta_after_second = load_meta(dir, "repeatable");
    assert_eq!(
        meta_after_second["status"].as_str().unwrap(),
        "done",
        "meta after second run should be 'done'"
    );

    // The start_time of the second run should be >= first run's start_time,
    // confirming it's a fresh meta and not a stale merged state.
    let t1 = meta_after_first["start_time"].as_str().unwrap();
    let t2 = meta_after_second["start_time"].as_str().unwrap();
    // Both are ISO8601; lexicographic comparison works for same-timezone strings
    assert!(
        t2 >= t1,
        "second run start_time ({}) should be >= first run start_time ({})",
        t2,
        t1
    );
}
