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
| 6 | `tail` reads to EOF then exits, not follow mode | main.rs:95-105 | Rewrote as follow mode: EOF -> check meta.status -> sleep 200ms -> retry |
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

---

## Review 2 — v2.0 depends_on + Wave Scheduling (2026-03-06)

**Reviewer:** Codex (GPT-5.4, xhigh reasoning)
**Scope:** Implementation plan review + 3 rounds of code review
**Tokens used:** ~156K across 3 review rounds

### Round 1 — Plan Review (pre-implementation)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | `wave` field on TaskMeta not backward-compatible — old meta.json fails to parse | Added `#[serde(default)]` on `wave` field |
| 2 | High | `interrupted` flag overloaded for Ctrl+C and task failure — blocks further Ctrl+C after first failure | Split into `wave_failed` and `sigint_received` flags |
| 3 | High | Future-wave tasks invisible to dashboard — totals wrong, watch exits early | Pre-create Pending meta files for all tasks before execution |
| 4 | Medium | `into_waves()` HashMap breaks YAML ordering within waves | Changed to index-keyed HashMap, sort ready indices by original position |
| 5 | Medium | `into_waves()` silently drops duplicate names via HashMap | Moved duplicate validation into `into_waves()` itself |
| 6 | Medium | Test suite missing edge cases: empty list, self-dep, duplicates | Added all missing test cases |
| 7 | Low | CLI help text still says "Launch all tasks in parallel" | Updated to "Launch tasks in dependency waves" |

### Round 2 — Code Review (post-implementation)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | JoinError (panic) path doesn't update meta on disk — stale Running state | Track wave task names, post-wave fixup writes Failed meta for unreported tasks |
| 2 | Medium | stdout-read error overwritten by exit code, reported as Done | Check `meta.error` before overwriting status; propagate error in TaskRunReport |
| 3 | Medium | Ctrl+C races with EOF in select — completed task misclassified as Cancelled | `biased` select prefers stdout over cancel |
| 4 | Medium | Dashboard alphabetical sort breaks wave-internal YAML order | Removed alphabetical sort (wave-internal order now follows YAML) |

### Round 3 — Final Review (post-fixes)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | JoinError synthetic report still doesn't write meta to disk | Added post-wave loop: scan for unreported tasks, load and update their meta files |
| 2 | Medium | Biased select can starve Ctrl+C on chatty stdout | Added inline `cancel.is_cancelled()` check after each stdout line |
| 3 | Medium | Dashboard relies on `read_dir()` order which is unspecified | Sort metas by `(wave, name)` for deterministic cross-wave display (note: display order differs from YAML order) |

### Final Verification

- 11 unit tests pass (into_waves: no deps, linear chain, diamond, circular, self-dep, unknown dep, duplicates, empty, backward compat, wave-0 order, later-wave order)
- Release build clean (only pre-existing warnings)
- Backward compatible: existing YAML without `depends_on` works identically (single wave)

---

## Review 3 — Phase 1 Refactoring (2026-03-07)

**Reviewer:** Codex (via codex-par, parallel review tasks)
**Scope:** All source files post-merge of refactor/phase1, refactor/typed-config, refactor/split-commands, refactor/test-harness
**Commits covered:** `d6f09d4` (runner safety hardening), `580ad88` (config hardening), `44ca6cb` + `15e4601` (integration test harness)
**Also merged:** `refactor/split-commands` — extracted `commands/{run,status,tail,clean}.rs` from `main.rs`; no review findings; `main.rs` now dispatches only

### Runner Safety Findings (commit `d6f09d4`)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | `kill_and_wait()` calls SIGTERM without first checking if the process already exited — races with natural exit | Added `try_wait()` before SIGTERM; skip signal if process already done |
| 2 | Medium | `timeout(SIGKILL_TIMEOUT, wait())` only handled `Ok(status)` — `Ok(Err(io_error))` and `Err(elapsed)` paths unhandled | Match all three cases: success, io error during wait, and timeout elapsed |
| 3 | Medium | `setpgid(0,0)` failure silently ignored in `pre_exec` | Return error from `pre_exec` closure so spawn propagates the failure |

### Config Hardening Findings (commit `580ad88`)

| # | Severity | Issue | Fix Applied |
|---|----------|-------|-------------|
| 1 | High | No task name validation — names with `/`, spaces, control chars, or `..` could cause path traversal or terminal injection | Added strict charset validation: `[A-Za-z0-9._-]` only |
| 2 | High | `result_file.to_str().unwrap()` panics on non-UTF-8 output paths | Replaced with `.ok_or_else(|| anyhow!(...))` to propagate error |
| 3 | Medium | Edge cases untested: `.`, `..`, control chars, spaces, unicode | Added test cases for all rejected patterns |

### Integration Test Harness (commits `44ca6cb`, `15e4601`)

| # | Status | Item |
|---|--------|------|
| 1 | Implemented | Fake `codex` binary for hermetic integration tests (no network, no real Codex) |
| 2 | Implemented | `test_single_task_success` — one task completes successfully end-to-end |
| 3 | Implemented | `test_single_task_failure` — failed task produces correct meta and exit code |
| 4 | Implemented | `test_wave_skip_on_failure` — downstream wave tasks marked Cancelled when wave fails |
| 5 | Implemented | `test_wave_sequential_execution` — second task start time ≥ first task end time |
| 6 | Implemented | `test_rerun_truncates_logs` — log file has exactly 2 lines after rerun |
| 7 | Implemented | Harness fails hard if fake_codex binary not built (removed SKIP guard) |

### Updated Final Verification

- 18 unit tests pass (11 wave-scheduling + 7 config/validation edge cases)
- 5 integration tests pass using fake_codex binary
- Release build clean
- Backward compatible: existing YAML without `depends_on` runs as single wave
