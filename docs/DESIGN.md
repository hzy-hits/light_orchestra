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

  ┌─ main ──────────────────────────────────────────────┐
  │  JoinSet + CancellationToken + tokio::select!       │
  │                                                     │
  │  ┌─ spawn task1 ──────────────────────────────────┐ │
  │  │  codex exec --json -C /path -s read-only ...   │ │
  │  │  ├─ stdout reader → JSONL parse → meta update  │ │
  │  │  ├─ stderr reader → drain to .stderr.log       │ │
  │  │  └─ main: select! { cancel, stdout_eof }       │ │
  │  └────────────────────────────────────────────────┘ │
  │                                                     │
  │  ┌─ spawn task2 ──────────────────────────────────┐ │
  │  │  (same structure, independent process)          │ │
  │  └────────────────────────────────────────────────┘ │
  │                                                     │
  │  ┌─ spawn dashboard ──────────────────────────────┐ │
  │  │  reads logs/*.meta.json every 1s               │ │
  │  │  renders TUI with status, tokens, last action  │ │
  │  └────────────────────────────────────────────────┘ │
  │                                                     │
  │  Ctrl+C → shutdown.cancel() → SIGTERM to all pgrps │
  └─────────────────────────────────────────────────────┘
```

### Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Parallel model | Independent `codex exec` processes | Bypasses MCP serial bottleneck and deadlock entirely |
| Async runtime | tokio JoinSet | Structured concurrency, `join_next()` + `select!` for Ctrl+C |
| Task runner sharing | `TaskRunner: Clone` | Lighter than `Arc`, each spawn gets own copy |
| Inter-process comms | File-based (meta.json) | Dashboard can be a separate command (`status`), not coupled to runner |
| Meta file writes | Atomic `.tmp` + `rename` | Prevents dashboard reading partial JSON |
| Process cleanup | `setpgid(0,0)` per child | Kill entire process group on Ctrl+C, no zombie leaks |
| stderr handling | Separate drain task to file | Prevents pipe buffer deadlock (4KB-64KB buffer fills → child blocks) |
| Event parsing | Unified `event.rs` | Single source of truth for runner, dashboard, and tail commands |
| Codex binary path | `CODEX_BIN` env var | Defaults to `/opt/homebrew/bin/codex`, portable across machines |
| Meta update frequency | Every 20 events | Balances disk IO cost vs dashboard realtime visibility |

## File Structure

```
src/
├── main.rs        CLI entry, JoinSet scheduling, Ctrl+C, tail follow mode
├── config.rs      YAML task config parsing (TasksConfig, TaskDef)
├── event.rs       Unified CodexEvent enum + JSONL parsing
├── meta.rs        TaskMeta with atomic save, TaskStatus enum
├── runner.rs      TaskRunner::run_task(), stderr drain, CancellationToken
└── dashboard.rs   TUI render, watch mode, UTF-8 safe truncate

Runtime generated:
├── outputs/       {task_name}.md — final Codex results
└── logs/          {task_name}.jsonl — full event stream
                   {task_name}.meta.json — live status for dashboard
                   {task_name}.stderr.log — stderr output
```

## Usage

### Task Configuration (YAML)

```yaml
tasks:
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
```

### Commands

```bash
# Launch all tasks in parallel with live dashboard
codex-par run tasks.yaml --dashboard

# Just launch (check results later)
codex-par run tasks.yaml

# Monitor running/completed tasks
codex-par status -w 3         # watch mode, refresh every 3s

# Follow a specific task's event stream
codex-par tail fee_calculator_v2

# Clean up
codex-par clean
```

### Dashboard Output

```
══════════════════════════════════════════════════════════════════════
  CODEX PARALLEL DASHBOARD  |  19:30:45  |  2/3 done, 1 running, 0 failed
══════════════════════════════════════════════════════════════════════

  ✓ fee_calculator_v2
    Status: done       Duration: 12m34s   Events: 303
    Tokens: 1100K in / 15K out    Files: 45  Cmds: 12
    Last: COMPLETED

  ✓ retry_job
    Status: done       Duration: 8m21s    Events: 187
    Tokens: 680K in / 9K out    Files: 28  Cmds: 8
    Last: COMPLETED

  ⟳ bff_judgement
    Status: running    Duration: 15m02s   Events: 282
    Tokens: 4712K in / 21K out    Files: 89  Cmds: 34
    Last: EXEC wc -c insurance-job/src/main/profiles/*/function.pro

────────────────────────────────────────────────────────────────────
  Results: outputs/*.md  |  Logs: logs/*.jsonl  |  Ctrl+C to cancel
```
