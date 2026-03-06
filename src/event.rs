/// Unified JSONL event parsing — single source of truth for runner, dashboard, and tail.
use serde_json::Value;

#[derive(Debug)]
pub enum CodexEvent {
    TokenCount { input_tokens: u64, output_tokens: u64 },
    ReadFile { file_path: String },
    ExecCommand { command: String },
    TaskComplete,
    Other(String),
}

impl CodexEvent {
    pub fn parse(line: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(line).ok()?;
        let evt_type = v.pointer("/payload/type")?.as_str()?;

        Some(match evt_type {
            "token_count" => {
                let usage = v.pointer("/payload/info/total_token_usage")?;
                CodexEvent::TokenCount {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                }
            }
            "read_file" => {
                let path = v.pointer("/payload/call/args/file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                CodexEvent::ReadFile { file_path: path }
            }
            "exec_command" => {
                let cmd = v.pointer("/payload/call/args/command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                CodexEvent::ExecCommand { command: cmd }
            }
            "task_complete" => CodexEvent::TaskComplete,
            other => CodexEvent::Other(other.to_string()),
        })
    }

    /// Extract short timestamp from raw JSONL line (HH:MM:SS)
    pub fn extract_timestamp(line: &str) -> &str {
        // "timestamp":"2026-03-06T10:59:16.169Z"  -> 10:59:16
        if let Some(pos) = line.find("\"timestamp\":\"") {
            let start = pos + 13; // skip `"timestamp":"`
            if line.len() >= start + 19 {
                return &line[start + 11..start + 19];
            }
        }
        ""
    }
}
