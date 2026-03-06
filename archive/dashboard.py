#!/usr/bin/env python3
"""
Codex Parallel Dashboard - 实时监控所有并行 Codex 任务
读取 logs/ 目录下的 .meta.json 和 .jsonl 文件，展示进度。

Usage:
    python3 dashboard.py              # 刷新一次
    python3 dashboard.py --watch      # 每 5 秒自动刷新
"""

import json
import sys
import os
import time
from pathlib import Path
from datetime import datetime

LOG_DIR = Path(__file__).parent / "logs"


def parse_jsonl_stats(jsonl_path: Path) -> dict:
    """从 JSONL 日志提取关键统计"""
    stats = {
        "events": 0,
        "files_read": [],
        "commands_run": [],
        "last_event_time": None,
        "input_tokens": 0,
        "output_tokens": 0,
        "last_action": "",
    }
    if not jsonl_path.exists():
        return stats

    for line in jsonl_path.open():
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue

        stats["events"] += 1
        stats["last_event_time"] = d.get("timestamp")
        payload = d.get("payload", {})

        if d.get("type") == "event_msg":
            evt = payload.get("type", "")
            if evt == "read_file":
                path = payload.get("call", {}).get("args", {}).get("file_path", "")
                if path:
                    stats["files_read"].append(path.split("/")[-1])
                    stats["last_action"] = f"READ {path.split('/')[-1]}"
            elif evt == "exec_command":
                cmd = payload.get("call", {}).get("args", {}).get("command", "")[:60]
                if cmd:
                    stats["commands_run"].append(cmd)
                    stats["last_action"] = f"EXEC {cmd[:50]}"
            elif evt == "token_count":
                info = payload.get("info", {}).get("total_token_usage", {})
                stats["input_tokens"] = info.get("input_tokens", 0)
                stats["output_tokens"] = info.get("output_tokens", 0)
            elif evt == "task_complete":
                stats["last_action"] = "COMPLETED"

    return stats


def render_dashboard():
    """渲染一次 dashboard"""
    if not LOG_DIR.exists():
        print("No logs/ directory found. Run tasks first.")
        return

    meta_files = sorted(LOG_DIR.glob("*.meta.json"))
    if not meta_files:
        print("No tasks found in logs/")
        return

    now = datetime.now().strftime("%H:%M:%S")
    print(f"\033[2J\033[H")  # clear screen
    print(f"{'='*70}")
    print(f"  CODEX PARALLEL DASHBOARD  |  {now}  |  {len(meta_files)} tasks")
    print(f"{'='*70}\n")

    for mf in meta_files:
        meta = json.loads(mf.read_text())
        name = meta["name"]
        status = meta.get("status", "unknown")
        start = meta.get("start_time", "")[:19]

        # 状态图标
        icon = {"running": "\033[33m⟳\033[0m", "done": "\033[32m✓\033[0m",
                "failed": "\033[31m✗\033[0m"}.get(status, "?")

        # 解析 JSONL 统计
        jsonl_path = LOG_DIR / f"{name}.jsonl"
        stats = parse_jsonl_stats(jsonl_path)

        # 计算运行时长
        duration = ""
        if start:
            try:
                st = datetime.fromisoformat(start)
                dur = datetime.now() - st
                mins = int(dur.total_seconds() // 60)
                secs = int(dur.total_seconds() % 60)
                duration = f"{mins}m{secs}s"
            except (ValueError, TypeError):
                pass

        tokens_k = stats["input_tokens"] / 1000
        print(f"  {icon} {name}")
        print(f"    Status: {status:<10} Duration: {duration:<10} Events: {stats['events']}")
        print(f"    Tokens: {tokens_k:.0f}K input / {stats['output_tokens']/1000:.0f}K output")
        print(f"    Files read: {len(stats['files_read'])}  Commands: {len(stats['commands_run'])}")
        print(f"    Last action: {stats['last_action'][:60]}")
        print()

    print(f"{'─'*70}")
    print(f"  Results: outputs/*.md  |  Logs: logs/*.jsonl")


def main():
    watch = "--watch" in sys.argv
    if watch:
        try:
            while True:
                render_dashboard()
                time.sleep(5)
        except KeyboardInterrupt:
            print("\nStopped.")
    else:
        render_dashboard()


if __name__ == "__main__":
    main()
