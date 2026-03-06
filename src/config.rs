use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct TasksConfig {
    pub tasks: Vec<TaskDef>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TaskDef {
    pub name: String,
    pub cwd: String,
    pub prompt: String,
    #[serde(default = "default_sandbox")]
    pub sandbox: String,
    pub model: Option<String>,
}

fn default_sandbox() -> String {
    "read-only".to_string()
}

impl TasksConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: TasksConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}
