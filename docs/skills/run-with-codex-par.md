---
name: run-with-codex-par
description: Use when running multiple Codex tasks in parallel — code review, refactoring, analysis across files or repos. Use instead of calling codex exec directly when you have 2+ independent tasks. Prevents MCP deadlock and serial bottleneck.
---

# Running Parallel Codex Tasks with codex-par

## Core principle

`codex-par` spawns N independent `codex exec --json` processes and runs them in dependency waves. Claude never blocks waiting — fire tasks, poll status, read results.

## Workflow

```
1. Write tasks.yaml
2. codex-par run tasks.yaml --dir ./run-YYYYMMDD/
3. Poll logs/*.meta.json until all terminal
4. Read outputs/*.md for results
```

## Step 1 — Write tasks.yaml

```yaml
tasks:
  - name: review-runner        # [A-Za-z0-9._-] only, used as filename
    cwd: /path/to/repo
    prompt: "Review src/runner.rs for correctness and safety issues. Report as markdown table."

  - name: review-config
    cwd: /path/to/repo
    prompt: "Review src/config.rs for type safety and validation gaps."

  - name: summarize            # runs after both reviews complete
    cwd: /path/to/repo
    prompt: "Summarize the findings from outputs/review-runner.md and outputs/review-config.md"
    depends_on: [review-runner, review-config]
```

**Rules:**
- `name`: `[A-Za-z0-9._-]` only — becomes the output filename
- `cwd`: absolute path where codex runs
- `sandbox`: `read-only` (default), `read-write`, or `network-read-only`
- `model`: optional, e.g. `o3` or `gpt-4o`
- `depends_on`: tasks in the same wave run parallel; dependent tasks wait

## Step 2 — Run

```bash
CODEX_BIN=/home/ivena/.nvm/versions/node/v20.19.5/bin/codex \
  /path/to/codex-par run tasks.yaml --dir ./run-$(date +%Y%m%d-%H%M)/
```

`codex-par` binary is at: `/home/ivena/coding/rust/light_orchestra/target/release/codex-par`
(build with `cargo build --release` in `/home/ivena/coding/rust/light_orchestra`)

`CODEX_BIN`: `/home/ivena/.nvm/versions/node/v20.19.5/bin/codex`

## Step 3 — Poll status

```bash
# Quick status check
cat ./run-dir/logs/*.meta.json | python3 -c "
import sys, json
for line in sys.stdin:
    try:
        m = json.loads(line)
        print(m.get('name','?'), m.get('status','?'), m.get('error',''))
    except: pass
"
```

Or use the built-in dashboard:
```bash
codex-par status --dir ./run-dir/
codex-par status --dir ./run-dir/ --watch 2   # auto-refresh every 2s
```

**Terminal statuses:** `done`, `failed`, `cancelled`
**Running statuses:** `pending`, `running`

Read a single meta file directly:
```bash
cat ./run-dir/logs/review-runner.meta.json
```

Key fields: `status`, `wave`, `error`, `input_tokens`, `output_tokens`, `start_time`, `end_time`, `last_action`

## Step 4 — Read results

```bash
cat ./run-dir/outputs/review-runner.md
cat ./run-dir/outputs/review-config.md
```

Or list all:
```bash
ls ./run-dir/outputs/
```

Use the Read tool to read output files into context.

## Tail a running task

```bash
codex-par tail review-runner --dir ./run-dir/
```

## Common patterns

**Parallel code review across files:**
```yaml
tasks:
  - name: review-A
    cwd: /repo
    prompt: "Review src/module_a.rs ..."
  - name: review-B
    cwd: /repo
    prompt: "Review src/module_b.rs ..."
  - name: review-C
    cwd: /repo
    prompt: "Review src/module_c.rs ..."
```

**Sequential pipeline (generate then review):**
```yaml
tasks:
  - name: implement
    cwd: /repo
    sandbox: read-write
    prompt: "Implement feature X in src/..."
  - name: review
    cwd: /repo
    prompt: "Review the implementation in src/... for correctness"
    depends_on: [implement]
```

**When Codex is slow:**
- Normal: 1–3 min per task (API latency + reasoning)
- `xhigh` reasoning takes longer but finds more issues
- Running tasks in parallel means total wall time ≈ slowest single task
- Nothing wrong — just LLM inference latency

## What codex-par solves

| Problem | Root cause | codex-par fix |
|---|---|---|
| MCP deadlock | Claude↔Codex double-blocking via same MCP channel | Independent processes, no shared MCP channel |
| Serial execution | Claude calls codex one at a time | Wave-parallel spawning |
| Zombie processes | No SIGTERM→SIGKILL on cancel | Process group kill on Ctrl+C |
| Stale logs on rerun | Append mode | Truncate on each run |
