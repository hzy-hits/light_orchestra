use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    Output,
    Patch,
    TestResult,
    Report,
    Log,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    Markdown,
    Json,
    Patch,
    PlainText,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Artifact {
    pub artifact_id: String,
    pub run_id: String,
    pub task_id: String,
    pub producer_agent: String,
    pub artifact_type: ArtifactType,
    pub format: ArtifactFormat,
    pub path: PathBuf,
    pub summary: String,
    #[serde(default)]
    pub references: Vec<String>,
    pub created_at: String,
}

impl Artifact {
    pub fn new_output(run_id: &str, task_id: &str, agent_id: &str, path: PathBuf) -> Self {
        Self {
            artifact_id: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            task_id: task_id.to_string(),
            producer_agent: agent_id.to_string(),
            artifact_type: ArtifactType::Output,
            format: ArtifactFormat::Markdown,
            path,
            summary: String::new(),
            references: Vec::new(),
            created_at: chrono::Local::now().to_rfc3339(),
        }
    }

    pub fn save_meta(&self, artifacts_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(artifacts_dir)?;

        let final_path = artifacts_dir.join(format!("{}.json", self.artifact_id));
        let tmp_path = artifacts_dir.join(format!("{}.json.tmp", self.artifact_id));
        let content = serde_json::to_string_pretty(self)?;

        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &final_path)?;

        Ok(())
    }
}
