# codex-par Refactoring Plan

> Designed by Codex (GPT-5.4, xhigh reasoning) | 2026-03-07
> Based on full source review of all src/ files, Cargo.toml, and docs/DESIGN.md

## Direction

The codebase is under-split, not over-split. Keep `event.rs` and `meta.rs` as distinct concepts, but split orchestration out of `main.rs`, split planning out of `config.rs`, and split data loading from rendering in `dashboard.rs`. No module merge is necessary; the main issue is that runtime, persistence, and presentation concerns are too entangled.

---

## Phase 1: Quick Wins / Safety Fixes

| Focus | Severity | Location | What to change | Why |
|---|---|---|---|---|
| Process cleanup on all paths | High | runner.rs:103, :164, :183, :195 | Introduce a single shutdown path that always sends SIGTERM, escalates to SIGKILL after a short timeout, and always `wait()`s to reap the child, even on cancellation or IO failure. | Current `?` and `break` paths can leave detached codex processes or zombies; the implementation does not fully meet the documented cleanup contract. |
| Stop swallowing operational errors | High | main.rs:79, runner.rs:40, dashboard.rs:141, meta.rs:97 | Replace `let _ =`, `.ok()`, and silent `if let Ok(...)` branches with `anyhow::Context` plus explicit policy: fail fast for required state, warn for best-effort UI paths. | Silent failure is the biggest observability gap in the code. Meta corruption or save/load failures currently produce misleading dashboard state. |
| Validate task inputs and remove stringly-typed config | Medium | config.rs:9, runner.rs:77, meta.rs:78, main.rs:232 | Change `cwd` to `PathBuf`, `sandbox` to an enum, and validate `task.name` as a safe file stem. Reject empty names/paths and invalid dependency targets early. | File names are derived directly from task names, which risks invalid paths, cross-platform breakage, and accidental path traversal. |
| Rerun semantics for logs | Medium | runner.rs:111, :128 | Stop opening `.jsonl` and `.stderr.log` in append mode for normal runs; either truncate or create per-run directories/run IDs. | Re-running the same task in the same `dir` currently mixes old and new logs, which makes `tail`, debugging, and dashboards unreliable. |
| Split `main.rs` command handling | Medium | main.rs:57, :247 | Extract `run_command`, `status_command`, `tail_command`, and `clean_command`; move `tail_follow` into a dedicated `tail.rs`. Return `ExitCode` instead of calling `std::process::exit`. | `main()` currently mixes CLI dispatch, orchestration, recovery, summary formatting, and tailing. It is the largest readability and testability hotspot. |
| End-to-end test harness | High | Cargo.toml, main.rs:61, runner.rs:36 | Add integration tests with a fake `CODEX_BIN` fixture that emits controlled JSONL/stdout/stderr and exit codes. Cover success, failure, skipped waves, and reruns. | The critical runtime path is almost entirely untested today; config tests alone do not protect the actual executor. |

## Phase 2: Moderate Refactors

| Focus | Severity | Location | What to change | Why |
|---|---|---|---|---|
| Centralize task lifecycle transitions | Medium | meta.rs:54, main.rs:76, runner.rs:37, :215 | Add methods like `pending()`, `start(pid)`, `record_event(...)`, `finish_done(...)`, `finish_failed(...)`, `finish_cancelled(...)`. Make `started_at` optional and remove dead/duplicated paths like `TaskMeta::fail`. | Status, timestamps, and error fields are mutated ad hoc across modules, which is already causing invariant drift and misleading timing semantics. |
| Split parsing from planning | Medium | config.rs:24, :40 | Keep `config.rs` as serde/input validation only, and move wave construction into `planner.rs` with an explicit plan type. Implement a true indegree-based Kahn planner. | `into_waves()` currently mixes deserialization concerns, validation, and scheduling. The code works, but the abstraction boundary is wrong. |
| Typed event parser | Medium | event.rs:13, :45, runner.rs:150, main.rs:279 | Replace `serde_json::Value` walking and string slicing with typed partial deserialization returning `Result<CodexEvent, EventParseError>`. | The current parser silently drops malformed lines, allocates more than necessary, and relies on brittle timestamp slicing. |
| Split dashboard store from renderer | Medium | dashboard.rs:9, :95, :130 | Move file loading/grouping/sorting into a `MetaStore` or `dashboard::store` module and keep terminal formatting in `dashboard::render`. Reuse the same snapshot instead of rescanning twice per watch interval. | `dashboard.rs` currently mixes storage, aggregation, formatting, and watch-loop control, and it does redundant disk reads. |
| Property and resilience tests | Medium | Cargo.toml, config.rs:40, event.rs:13, dashboard.rs:130, main.rs:252 | Add `proptest` for DAGs and event lines; add integration tests for corrupt meta files, missing logs, and `tail` starting before a log file exists. | The scheduler and parser are both good candidates for property testing, and the file-driven UX needs regression coverage around partial/corrupt state. |

## Phase 3: Larger Architectural Refactors

| Focus | Severity | Location | What to change | Why |
|---|---|---|---|---|
| Introduce explicit services | High | main.rs:60, runner.rs:23, meta.rs:54, dashboard.rs:9 | Refactor into `Planner`, `Executor`, `MetaStore`, and `DashboardService`, with CLI handlers only wiring dependencies. | This is the cleanest long-term shape if the tool grows retries, concurrency limits, alternate UIs, or a richer status API. |
| Separate runtime state from persisted schema | Medium | runner.rs:71, meta.rs:5, dashboard.rs:130, event.rs:4 | Add an internal `TaskSnapshot`/`TaskState` model and treat `TaskMeta` as a persisted view, not the in-memory source of truth. | One struct is currently doing too much: runtime state, serialized file format, and dashboard view model. That coupling is constraining the design. |
| Narrow `anyhow` to the CLI boundary | Medium | config.rs:25, event.rs:13, meta.rs:77, runner.rs:71 | Introduce module-specific error enums and use `anyhow` only in `main`/command handlers. | Once the modules are split, typed errors will make executor/store/parser behavior far easier to reason about and test. |
| Regression tests for cancellation and performance baselines | Medium | Cargo.toml, runner.rs:93, event.rs:13, meta.rs:77 | Add tests where the fake codex spawns grandchildren and ignores SIGTERM briefly; add benchmarks for event parsing and meta write frequency. | The highest-risk production behavior is cancellation/process cleanup. After phase 1 and 2, lock it down with regression tests and light benchmarks. |
