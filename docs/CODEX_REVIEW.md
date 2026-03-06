# Codex Code Review Record

## Review 1 — Initial Implementation (2026-03-06)

**Reviewer:** Codex (GPT-5.4, xhigh reasoning)
**Scope:** All source files in src/ + Cargo.toml

### High-Risk Findings

| # | Issue | Location | Impact | Fix |
|---|-------|----------|--------|-----|
| 1 | stderr piped but no consumer — pipe buffer fills, child process hangs | runner.rs:44 | Task silently freezes | Added separate stderr drain task writing to `{name}.stderr.log` |
| 2 | meta.json direct overwrite, not atomic — dashboard reads partial JSON | meta.rs:61 | Dashboard crashes or silently drops tasks | Changed to `.tmp` + `rename` atomic write |
| 3 | Only success path writes final status — any error leaves status=running forever | runner.rs:30-86 | Task appears stuck when it actually failed | Wrapped `run_task_inner` with outer error handler that always updates meta |
| 4 | Ctrl+C only kills codex parent — spawned child processes leak | main.rs:65, runner.rs:46 | Zombie codex processes consume resources | Added `setpgid(0,0)` per child + `kill(-pgid, SIGTERM)` on cancel |
| 5 | `truncate()` slices by byte offset — panics on multi-byte UTF-8 (Chinese) | dashboard.rs:91 | Dashboard crashes on Chinese last_action | Changed to `chars().take(n).collect()` |

### Medium-Risk Findings

| # | Issue | Location | Fix |
|---|-------|----------|-----|
| 6 | `tail` reads to EOF then exits, not follow mode | main.rs:95-105 | Rewrote as follow mode: EOF → check meta.status → sleep 200ms → retry |
| 7 | `create_dir_all().ok()` swallows errors | runner.rs:21-22 | Changed `TaskRunner::new` to return `Result`, propagate errors |

### Architecture Suggestions (Implemented)

1. **New `event.rs` module** — unified JSONL event parsing, avoids 3 places hand-writing JSON pointers
2. **`TaskRunner: Clone`** instead of `Arc<TaskRunner>` — each spawn gets lightweight copy
3. **`JoinSet` + `CancellationToken`** for scheduling — not manual `Vec<JoinHandle>`
4. **`TaskStatus::Cancelled`** — added to distinguish user cancellation from failure
5. **`exit_code` and `error` fields on TaskMeta** — better post-mortem debugging
6. **`CODEX_BIN` env var** — portability across machines and test environments
7. **`dashboard::watch_until_cancelled`** — accepts CancellationToken, exits cleanly with tasks

### Architecture Suggestions (Not Yet Implemented)

- Retry logic: optional, only for infrastructure failures (spawn/IO/transient non-zero exit), not "model answered poorly"
- Integration tests: fake codex binary, Ctrl+C cancel test, concurrent meta read/write test

### Codex's Recommended `main.rs` Pattern

```rust
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

let shutdown = CancellationToken::new();
let mut set = JoinSet::new();

for task in tasks {
    let runner = runner.clone();
    let shutdown = shutdown.clone();
    set.spawn(async move { runner.run_task(task, shutdown).await });
}

loop {
    tokio::select! {
        _ = tokio::signal::ctrl_c(), if !interrupted => {
            interrupted = true;
            shutdown.cancel();
        }
        Some(joined) = set.join_next() => {
            reports.push(joined??);
        }
        else => break,
    }
}
```

This pattern was adopted in our implementation.

## Review 2 — Pending

Second review was attempted but blocked by the MCP deadlock issue documented in `MCP_DEADLOCK_ANALYSIS.md`. To be completed after testing the tool end-to-end.
