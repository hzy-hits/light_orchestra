# codex-par Design Document

## Problem Statement

Claude Code calls Codex via MCP (Model Context Protocol) for code analysis tasks. Two critical issues exist:

1. **Serial Execution**: All Codex MCP calls from Claude are serialized — even when using multiple team agents, they share one MCP server and queue up. A 3-task workload that could finish in 30min takes 90min.

2. **MCP Deadlock**: Claude's MCP implementation is half-duplex synchronous blocking. When Claude sends a request to Codex and blocks waiting for the response, it stops processing inbound messages. If Codex needs Claude to respond to a sub-request or notification, both sides deadlock — alive but waiting for the other to move first.

3. **No Process Visibility**: When Codex runs via MCP, all intermediate events (files read, commands executed, reasoning steps) are invisible. Only the final result is returned. A single task can consume 4.7M+ input tokens over 30+ minutes with zero progress feedback.

## Solution

`codex-par` bypasses the MCP layer entirely. Each task spawns an independent `codex exec --json` process, achieving true process-isolated parallelism with live monitoring.

## Architecture

```
codex-par run tasks.yaml --dashboard

  +-- main ------------------------------------------------+
  |  for wave in waves:                                     |
  |    JoinSet + CancellationToken + tokio::select!         |
  |                                                         |
  |    +-- spawn task1 (wave 0) --------------------------+ |
  |    |  codex exec --json -C /path -s read-only ...     | |
  |    |  +- stdout reader -> JSONL parse -> meta update  | |
  |    |  +- stderr reader -> drain to .stderr.log        | |
  |    |  +- biased select! { stdout, cancel }            | |
  |    +--------------------------------------------------+ |
  |                                                         |
  |    +-- spawn task2 (wave 0) --------------------------+ |
  |    |  (same structure, independent process)            | |
  |    +--------------------------------------------------+ |
  |                                                         |
  |    wave 0 completes -> wave 1 starts                    |
  |                                                         |
  |    +-- spawn task3 (wave 1, depends on wave 0) -------+ |
  |    |  (same structure)                                 | |
  |    +--------------------------------------------------+ |
  |                                                         |
  |  +-- dashboard ----------------------------------------+ |
  |  |  reads logs/*.meta.json every 1s                    | |
  |  |  renders TUI grouped by wave                        | |
  |  +----------------------------------------------------+ |
  |                                                         |
  |  Ctrl+C -> shutdown.cancel() -> SIGTERM -> 3s -> SIGKILL |
  |  (Unix: kill(-pgid,SIGTERM); non-Unix: child.kill())     |
  |  Wave failure -> skip subsequent waves                  |
  +---------------------------------------------------------+
```

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Parallel model | Independent `codex exec` processes | Bypasses MCP serial bottleneck and deadlock entirely |
| Wave scheduling | Kahn's algorithm topological sort | `depends_on` DAG partitioned into waves; parallel within, serial between |
| Async runtime | tokio JoinSet per wave | Structured concurrency, `join_next()` + `select!` for Ctrl+C |
| Select bias | `biased` select, stdout first | Prevents Ctrl+C from misclassifying completed tasks as cancelled |
| Cancel responsiveness | Inline `is_cancelled()` check | Ensures chatty tasks still respond to Ctrl+C promptly |
| Interrupt flags | Separate `wave_failed` / `sigint_received` | Correct cancellation reasons; Ctrl+C still works after a task failure |
| Pre-created metas | All tasks get Pending meta before execution | Dashboard shows full pipeline from the start |
| Panic recovery | Post-wave meta fixup | JoinError path writes terminal meta to disk, prevents stale Running state |
| Task runner sharing | `TaskRunner: Clone` | Lighter than `Arc`, each spawn gets own copy |
| Inter-process comms | File-based (meta.json) | Dashboard can be a separate command (`status`), not coupled to runner |
| Meta file writes | Atomic `.tmp` + `rename` | Prevents dashboard reading partial JSON |
| Process cleanup | `setpgid(0,0)` per child | Kill entire process group on Ctrl+C, no zombie leaks |
| stderr handling | Separate drain task to file | Prevents pipe buffer deadlock (4KB-64KB buffer fills -> child blocks) |
| Event parsing | Unified `event.rs` | Shared by runner and `tail`; dashboard reads `meta.json` (not JSONL) |
| Codex binary path | `CODEX_BIN` env var | Default `/opt/homebrew/bin/codex` is macOS/Homebrew-specific; override with `CODEX_BIN` for other platforms |
| Meta update frequency | Every 20 events | Balances disk IO cost vs dashboard realtime visibility |
| Execution order | Index-keyed Kahn's algorithm | Tasks within each wave execute in YAML input order |
| Display order | Dashboard sorts by `(wave, name)` | Deterministic display across platforms; display may differ from YAML order |

## File Structure

```
src/
+-- main.rs               CLI entry, subcommand dispatch only
+-- commands/
|   +-- run.rs            Wave scheduling, JoinSet, Ctrl+C, panic recovery
|   +-- status.rs         Status display and watch mode
|   +-- tail.rs           Follow mode: poll JSONL until task terminal
|   +-- clean.rs          Remove outputs/, logs/
+-- config.rs             YAML config parsing, depends_on, into_waves() topological sort
+-- event.rs              Unified CodexEvent enum + JSONL parsing
+-- meta.rs               TaskMeta with wave field, atomic save, TaskStatus enum
+-- runner.rs             TaskRunner::run_task(), biased select, stderr drain, cancel
+-- dashboard.rs          TUI render grouped by wave, watch mode, UTF-8 safe truncate

Runtime generated:
+-- outputs/       {task_name}.md -- final Codex results
+-- logs/          {task_name}.jsonl -- full event stream
                   {task_name}.meta.json -- live status for dashboard
                   {task_name}.stderr.log -- stderr output
```

## Usage

### Task Configuration (YAML)

```yaml
tasks:
  # Wave 0: no dependencies, run in parallel
  - name: "fee_calculator_v2"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Analyze InsuranceFeeCalculatorV2Facade...

  - name: "retry_job"
    cwd: "/path/to/project"
    sandbox: "read-only"
    model: "gpt-5.4"        # optional model override
    prompt: |
      Analyze InsureAndClaimFailedRetryJob...

  # Wave 1: depends on both Wave 0 tasks
  - name: "cross_review"
    depends_on:
      - "fee_calculator_v2"
      - "retry_job"
    cwd: "/path/to/project"
    sandbox: "read-only"
    prompt: |
      Read outputs/ and cross-review both analysis reports...
      # Note: outputs/ is under the --dir base directory, not under cwd.
      # If prompts refer to outputs/ relatively, set --dir to the same path as cwd.
```

### Task Schema and Validation

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | Must match `[A-Za-z0-9._-]`; no spaces, slashes, or control characters |
| `prompt` | string | yes | Passed to `codex exec` as the task prompt |
| `cwd` | path | yes | Working directory for the Codex process |
| `sandbox` | string | no | `read-only` (default), `read-write`, or `network-read-only` |
| `model` | string | no | Override Codex model for this task |
| `depends_on` | list | no | Task names this task must wait for; must all be known names |

Validation errors are caught before any tasks start:
- Duplicate task names are rejected
- Unknown `depends_on` names are rejected
- Circular dependencies are detected
- Task names with invalid characters are rejected (`.` and `..` are also explicitly rejected)
- Empty `cwd` is rejected

### Commands

```bash
# Launch tasks in dependency waves with live dashboard
codex-par run tasks.yaml --dashboard

# Launch with a custom output directory (default: cwd of the process)
codex-par run tasks.yaml --dir /path/to/workdir

# Just launch (check results later)
codex-par run tasks.yaml

# Monitor running/completed tasks (exits when all tasks reach terminal state)
codex-par status -w 3         # watch mode, refresh every 3s

# Follow a specific task's event stream (polls until task reaches terminal state,
# then exits; prints normalized event summaries, not raw JSONL)
codex-par tail fee_calculator_v2

# All subcommands accept --dir / -d to set the base directory for outputs/ and logs/
codex-par clean --dir /path/to/workdir
```

### Dashboard Output

```
========================================================================
  CODEX PARALLEL DASHBOARD  |  19:30:45  |  3/4 done, 1 running, 0 failed
========================================================================

  -- Wave 0 (2/2 done) --
    ✓ fee_calculator_v2
      Status: done       Duration: 12m34s   Events: 303
      Tokens: 1100K in / 15K out    Files: 45  Cmds: 12
      Last: COMPLETED

    ✓ retry_job
      Status: done       Duration: 8m21s    Events: 187
      Tokens: 680K in / 9K out    Files: 28  Cmds: 8
      Last: COMPLETED

  -- Wave 1 (1/2 done) --
    ⟳ cross_review
      Status: running    Duration: 5m02s    Events: 82
      Tokens: 412K in / 6K out    Files: 12  Cmds: 3
      Last: READ fee_calculator_v2.md

    ◯ final_report
      Status: pending    Duration: -        Events: 0
      Tokens: 0K in / 0K out    Files: 0  Cmds: 0
      Last:

------------------------------------------------------------------------
  Results: outputs/*.md  |  Logs: logs/*.jsonl  |  Ctrl+C to cancel
```

## Wave Scheduling

Tasks with `depends_on` are partitioned into waves using Kahn's algorithm:

1. Wave 0: all tasks with no dependencies (parallel)
2. Wave 1: tasks whose dependencies are all in Wave 0 (parallel)
3. Wave N: tasks whose dependencies are all in Wave < N (parallel)

Behavior:
- Circular dependencies are detected and reported as errors
- Unknown dependency names are rejected at parse time
- Duplicate task names are rejected
- If any task in a wave fails, all subsequent waves are skipped (tasks marked Cancelled)
- Ctrl+C cancels all running tasks and skips remaining waves
- Tasks within each wave are scheduled in YAML input order (execution order)
- The dashboard displays tasks sorted by `(wave, name)` — display order may differ from YAML order

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Task fails (non-zero exit) | Wave marked failed, subsequent waves skipped |
| stdout read error | Task marked Failed (not overwritten by exit code) |
| Task panics (JoinError) | Meta updated to Failed on disk, synthetic report created |
| Ctrl+C during wave | All tasks sent SIGTERM to process group; after 3s, SIGKILL escalation (Unix only); non-Unix falls back to `child.kill()`; remaining waves cancelled |
| Ctrl+C after task failure | Still works (separate `sigint_received` flag) |
| Chatty task ignoring cancel | Inline `is_cancelled()` check between stdout lines |
