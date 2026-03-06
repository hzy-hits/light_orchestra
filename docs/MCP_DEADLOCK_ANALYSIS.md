# Claude ↔ Codex MCP Deadlock Analysis

## Observed Symptom

Claude Code calls `mcp__codex__codex` and hangs indefinitely.

**Process state when stuck:**
- Claude PID (s006): 16% CPU, alive but not progressing
- Codex MCP server: 0% CPU, all threads on condition variables (idle, waiting for parent)
- Latest session JSONL stops being written
- Codex is not stuck — it's waiting for Claude. Claude is the one hung.

## Most Likely Root Cause: Half-Duplex Synchronous Blocking (Hypothesis)

```
Claude ──request──> Codex MCP Server
  │                      │
  │  (blocking await)    │  (may send sub-request back)
  │                      │
  └── stops processing   │  waits for Claude to respond
      inbound messages <─┘
         ═══ DEADLOCK ═══
```

Claude's MCP client implementation:
1. Sends request to Codex MCP server via stdio
2. Blocks the calling thread/task waiting for the final response
3. **Stops processing any inbound messages** during the wait
4. If Codex sends a **server-initiated request** that needs a reply (e.g. `sampling/createMessage`) → both sides deadlock (neither will yield to let the other proceed)
   - Note: progress is a one-way *notification* and alone does not cause mutual deadlock — but a full write buffer from accumulated notifications can still block on a full pipe
   - "Heartbeat" is not a standard MCP term; if a `ping` request/response is required, that too causes deadlock under a half-duplex reader

## The 4 Common Causes

| # | Cause | Description |
|---|-------|-------------|
| 1 | Reader/sender coupled | Same thread handles "send request + wait" AND "read inbound messages" |
| 2 | Lock held during await | Session/writer/state lock held while awaiting response |
| 3 | Half-duplex implementation | Protocol is full-duplex (JSON-RPC), but implementation only handles one direction at a time |
| 4 | Pipe buffer exhaustion | stdout/stderr not concurrently drained → OS pipe buffer (4-64KB) fills → child process blocks on write → looks like protocol deadlock |

**Most likely cause in our case is #1**: Codex process is idle waiting for messages, Claude is alive but not pushing new requests. This matches the half-duplex pattern where the "wait for response" code blocks the same path that should process inbound messages. Without a protocol trace or thread dump from the Claude MCP client, this remains a hypothesis — the observed symptoms prove a stalled bidirectional session, not which internal mechanism caused it.

## Correct Architecture: Actor/Event-Loop Model

```
┌─────────────────────────────────────────────────┐
│                Business Logic                    │
│  send_request(msg)                               │
│    → register pending[request_id] = oneshot_tx   │
│    → enqueue msg to outbound channel             │
│    → await oneshot_rx  (NOT blocking reader)      │
└─────────────────────────────────────────────────┘
        │ outbound channel              ▲ dispatch by request_id
        ▼                               │
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│ Writer Task  │  │ Reader Task  │  │ Stderr Task  │
│ stdin serial │  │ stdout parse │  │ stderr → log │
│ write        │  │ dispatch msg │  │ file         │
└──────────────┘  └──────────────┘  └──────────────┘
```

### WRONG pattern (deadlock-prone):
```rust
lock_session();
write_request_to_stdin();
loop { read_stdout_until_response(); }  // blocks reader for everything else
unlock_session();
```

### CORRECT pattern:
```rust
let (tx, rx) = oneshot::channel();
pending_map.insert(request_id, tx);
outbound_channel.send(request).await;
let response = rx.await;  // reader task will send response here
// reader task continues processing all other messages independently
```

## Hard Rules to Prevent MCP Deadlock

1. **Read loop must be independent** — never block the reader waiting for a specific response
2. **Write loop independent** — use channel, don't let multiple callers race on stdin
3. **Never await while holding a lock** — lock only protects in-memory state mutation
4. **While waiting for response, still process**: progress, notifications, tool calls, heartbeats, cancellations
5. **Forbid implicit callback cycles** — or explicitly support reentrant RPC
6. **Drain stderr separately** — always, unconditionally
7. **Timeout + watchdog per request** — safety net for when all else fails

## Serial Execution and Contention Evidence

*(Separate from the deadlock/hang analysis above — this section covers throughput degradation under the shared MCP server.)*

Session timeline analysis from 2026-03-06 shows heavy contention (note: exact timestamps show some overlap, so this is evidence of *limited concurrency*, not strict serialization):

```
06:45 ────────────────────────────────────────────── 11:01

#1 team-qa-skills   |███████████████████████████████| 07:02 → 10:50
#2 lifeevent-ins    |         ████████|               09:55 → 10:32
#3 faas-life (BFF)  |              ██████████████████| 10:43 → 11:01

Tests:                       |x| |x| |x|             10:10-10:18
```

- #2 and #3 overlap with #1, so throughput is not strictly serial — but throughput is far below what independent processes achieve
- Tasks that should have run in parallel were heavily queued through the shared MCP server
- Only independently launched `codex` CLI processes (separate terminals) achieved true parallelism

> **Note:** The "confirmed serialization" framing from the original observation may overstate the case; "severe MCP contention / limited concurrency" is a more accurate characterization given the timestamp overlap.

## Recovery Procedure

**Don't just kill Codex — kill the entire Claude session chain:**

```bash
# Find the stuck Claude session on a specific tty
ps aux | grep claude | grep s006

# Kill the full chain: Claude + MCP wrapper + MCP server + any child
kill 17875 18009 18010 18011 18044

# Keep other Claude sessions alive
# (e.g., 86075, 86107, 86110 on s007)
```

Killing only the Codex MCP server doesn't help because Claude is the side that's hung — it won't recover by itself when the server restarts.

## Our Solution: Bypass MCP Entirely

`codex-par` eliminates all these issues by spawning independent `codex exec` processes:

| MCP Approach | codex-par Approach |
|---|---|
| Bidirectional RPC (deadlock risk) | No re-entrant RPC after spawn; parent drains stdout and stderr concurrently; OS signals for cancellation |
| Synchronous blocking await | Async `tokio::select!` + independent reader tasks |
| Shared MCP server (serial queue) | Each task is an independent OS process |
| stdout/stderr may interlock | stderr drained to separate file |
| No timeout/watchdog | Manual cancellation (Ctrl+C) → SIGTERM → SIGKILL; no automatic per-task timeout yet |
| No progress visibility | Live JSONL streaming + dashboard |
