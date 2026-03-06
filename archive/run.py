#!/usr/bin/env python3
"""
Codex Parallel Runner - 真正的多进程并行 Codex 任务执行器
每个任务独立 codex exec 进程，互不阻塞，实时可监控。

Usage:
    python3 run.py tasks.yaml
    python3 run.py tasks.yaml --dashboard  # 启动后自动打开监控
"""

import subprocess
import os
import sys
import json
import time
import signal
import yaml
from pathlib import Path
from datetime import datetime
from concurrent.futures import ProcessPoolExecutor

# ─── Constants ───
CODEX_BIN = "/opt/homebrew/bin/codex"
OUTPUT_DIR = Path(__file__).parent / "outputs"
LOG_DIR = Path(__file__).parent / "logs"


def run_single_task(task: dict) -> dict:
    """在独立进程中运行一个 Codex 任务"""
    name = task["name"]
    cwd = task["cwd"]
    prompt = task["prompt"]
    sandbox = task.get("sandbox", "read-only")
    model = task.get("model", None)

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    LOG_DIR.mkdir(parents=True, exist_ok=True)

    result_file = OUTPUT_DIR / f"{name}.md"
    log_file = LOG_DIR / f"{name}.jsonl"
    meta_file = LOG_DIR / f"{name}.meta.json"

    # 写入元数据（供 dashboard 读取）
    meta = {
        "name": name,
        "cwd": cwd,
        "prompt": prompt[:200],
        "status": "running",
        "pid": None,
        "start_time": datetime.now().isoformat(),
        "end_time": None,
    }
    meta_file.write_text(json.dumps(meta, ensure_ascii=False, indent=2))

    # 构建命令
    cmd = [
        CODEX_BIN, "exec",
        "-C", cwd,
        "-s", sandbox,
        "-o", str(result_file),
        "--json",                   # JSONL 流输出到 stdout
        "--skip-git-repo-check",
        prompt,
    ]
    if model:
        cmd.insert(2, "-m")
        cmd.insert(3, model)

    # TODO(human): 实现进程事件流的处理逻辑
    # 当前框架会启动 codex exec 并把 JSONL 流写入 log_file，
    # 但你需要决定：如何从 JSONL 流中提取关键事件并更新 meta_file？
    # 提示：event types 包括 exec_command, read_file, message, token_count 等
    pass

    return {"name": name, "result_file": str(result_file)}


def load_tasks(config_path: str) -> list:
    """从 YAML 配置加载任务列表"""
    with open(config_path) as f:
        config = yaml.safe_load(f)
    return config.get("tasks", [])


def main():
    if len(sys.argv) < 2:
        print("Usage: python3 run.py tasks.yaml")
        sys.exit(1)

    config_path = sys.argv[1]
    tasks = load_tasks(config_path)
    print(f"Loaded {len(tasks)} tasks, launching in parallel...")

    with ProcessPoolExecutor(max_workers=len(tasks)) as executor:
        futures = {executor.submit(run_single_task, t): t["name"] for t in tasks}
        for future in futures:
            try:
                result = future.result()
                print(f"[DONE] {result['name']} -> {result['result_file']}")
            except Exception as e:
                print(f"[FAIL] {futures[future]}: {e}")


if __name__ == "__main__":
    main()
