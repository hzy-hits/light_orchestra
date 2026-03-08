use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Instruction,
    Observation,
    ArtifactRef,
    Critique,
    Decision,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageEnvelope {
    pub message_id: String,
    pub run_id: String,
    pub task_id: String,
    pub thread_id: String,
    pub from_agent: String,
    pub to_agent: String,
    pub msg_type: MessageType,
    #[serde(default)]
    pub parent_message_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Local>,
    pub body: serde_json::Value,
}

impl MessageEnvelope {
    pub fn new(
        run_id: impl Into<String>,
        task_id: impl Into<String>,
        thread_id: impl Into<String>,
        from_agent: impl Into<String>,
        to_agent: impl Into<String>,
        msg_type: MessageType,
        body: serde_json::Value,
    ) -> Self {
        Self {
            message_id: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.into(),
            task_id: task_id.into(),
            thread_id: thread_id.into(),
            from_agent: from_agent.into(),
            to_agent: to_agent.into(),
            msg_type,
            parent_message_id: None,
            created_at: chrono::Local::now(),
            body,
        }
    }
}

pub fn append_jsonl(path: &std::path::Path, msg: &MessageEnvelope) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    serde_json::to_writer(&mut file, msg)?;
    file.write_all(b"\n")?;
    Ok(())
}
