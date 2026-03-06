use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Sandbox {
    ReadOnly,
    ReadWrite,
    NetworkReadOnly,
}

impl Default for Sandbox {
    fn default() -> Self {
        Sandbox::ReadOnly
    }
}

impl std::fmt::Display for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sandbox::ReadOnly => write!(f, "read-only"),
            Sandbox::ReadWrite => write!(f, "read-write"),
            Sandbox::NetworkReadOnly => write!(f, "network-read-only"),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TasksConfig {
    pub tasks: Vec<TaskDef>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TaskDef {
    pub name: String,
    pub cwd: std::path::PathBuf,
    pub prompt: String,
    #[serde(default)]
    pub sandbox: Sandbox,
    pub model: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl TasksConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: TasksConfig = serde_yaml::from_str(&content)?;
        for task in &config.tasks {
            anyhow::ensure!(
                !task.cwd.as_os_str().is_empty(),
                "task '{}' has empty cwd",
                task.name
            );
        }
        Ok(config)
    }

    /// Partition tasks into execution waves using topological sort (Kahn's algorithm).
    ///
    /// Returns `Vec<Vec<TaskDef>>` where wave[0] has no dependencies, wave[1] depends
    /// only on wave[0] tasks, etc. Task order within each wave preserves YAML input order.
    ///
    /// Errors:
    /// - Duplicate task names
    /// - Unknown task name in depends_on
    /// - Circular dependency detected
    pub fn into_waves(self) -> anyhow::Result<Vec<Vec<TaskDef>>> {
        use std::collections::{HashMap, HashSet};

        // Validate: no duplicate task names
        let mut seen = HashSet::new();
        for task in &self.tasks {
            anyhow::ensure!(
                seen.insert(&task.name),
                "duplicate task name: '{}'",
                task.name
            );
        }

        // Validate task names are safe file stems (no path separators, not empty)
        for task in &self.tasks {
            anyhow::ensure!(
                !task.name.is_empty(),
                "task name cannot be empty"
            );
            anyhow::ensure!(
                !task.name.contains('/') && !task.name.contains('\\') && !task.name.contains('\0'),
                "task name '{}' contains invalid characters (/, \\, or null)",
                task.name
            );
            anyhow::ensure!(
                task.name != "." && task.name != "..",
                "task name '{}' is not a valid file stem",
                task.name
            );
        }

        // Validate: all depends_on references exist
        let task_names: HashSet<&str> = self.tasks.iter().map(|t| t.name.as_str()).collect();
        for task in &self.tasks {
            for dep in &task.depends_on {
                anyhow::ensure!(
                    task_names.contains(dep.as_str()),
                    "task '{}' depends on unknown task '{}'",
                    task.name,
                    dep
                );
            }
        }

        // Build index-keyed structures to preserve YAML order
        let mut remaining: HashMap<usize, TaskDef> = self
            .tasks
            .into_iter()
            .enumerate()
            .map(|(i, t)| (i, t))
            .collect();
        let mut placed: HashSet<String> = HashSet::new();
        let mut waves: Vec<Vec<TaskDef>> = Vec::new();

        while !remaining.is_empty() {
            // Collect ready indices, sorted by original order
            let mut ready: Vec<usize> = remaining
                .iter()
                .filter(|(_, t)| t.depends_on.iter().all(|d| placed.contains(d)))
                .map(|(idx, _)| *idx)
                .collect();
            ready.sort();

            anyhow::ensure!(
                !ready.is_empty(),
                "circular dependency detected among tasks: {:?}",
                remaining.values().map(|t| &t.name).collect::<Vec<_>>()
            );

            let mut wave = Vec::new();
            for idx in &ready {
                let task = remaining.remove(idx).unwrap();
                placed.insert(task.name.clone());
                wave.push(task);
            }
            waves.push(wave);
        }

        Ok(waves)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(name: &str, depends_on: Vec<&str>) -> TaskDef {
        TaskDef {
            name: name.to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            prompt: "test".to_string(),
            sandbox: Sandbox::default(),
            model: None,
            depends_on: depends_on.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn no_dependencies_single_wave() {
        let config = TasksConfig {
            tasks: vec![task("a", vec![]), task("b", vec![]), task("c", vec![])],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 3);
        // Preserves YAML order
        assert_eq!(waves[0][0].name, "a");
        assert_eq!(waves[0][1].name, "b");
        assert_eq!(waves[0][2].name, "c");
    }

    #[test]
    fn linear_chain_three_waves() {
        let config = TasksConfig {
            tasks: vec![
                task("a", vec![]),
                task("b", vec!["a"]),
                task("c", vec!["b"]),
            ],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0][0].name, "a");
        assert_eq!(waves[1][0].name, "b");
        assert_eq!(waves[2][0].name, "c");
    }

    #[test]
    fn diamond_dependency() {
        let config = TasksConfig {
            tasks: vec![
                task("a", vec![]),
                task("b", vec!["a"]),
                task("c", vec!["a"]),
                task("d", vec!["b", "c"]),
            ],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].len(), 1); // a
        assert_eq!(waves[1].len(), 2); // b, c
        assert_eq!(waves[2].len(), 1); // d
        assert_eq!(waves[2][0].name, "d");
    }

    #[test]
    fn circular_dependency_error() {
        let config = TasksConfig {
            tasks: vec![task("a", vec!["b"]), task("b", vec!["a"])],
        };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn self_dependency_error() {
        let config = TasksConfig {
            tasks: vec![task("a", vec!["a"])],
        };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn unknown_dependency_error() {
        let config = TasksConfig {
            tasks: vec![task("a", vec!["nonexistent"])],
        };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("unknown task"));
    }

    #[test]
    fn duplicate_names_error() {
        let config = TasksConfig {
            tasks: vec![task("a", vec![]), task("a", vec![])],
        };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("duplicate task name"));
    }

    #[test]
    fn empty_task_list() {
        let config = TasksConfig { tasks: vec![] };
        let waves = config.into_waves().unwrap();
        assert!(waves.is_empty());
    }

    #[test]
    fn backward_compat_no_depends_on() {
        let yaml = r#"
tasks:
  - name: "x"
    cwd: "/tmp"
    prompt: "hello"
"#;
        let config: TasksConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.tasks[0].depends_on.is_empty());
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 1);
    }

    #[test]
    fn preserves_order_within_wave() {
        let config = TasksConfig {
            tasks: vec![
                task("z", vec![]),
                task("m", vec![]),
                task("a", vec![]),
            ],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves[0][0].name, "z");
        assert_eq!(waves[0][1].name, "m");
        assert_eq!(waves[0][2].name, "a");
    }

    #[test]
    fn preserves_order_in_later_waves() {
        // Wave 0: root
        // Wave 1: z, m, a (should preserve YAML order, not alphabetical)
        let config = TasksConfig {
            tasks: vec![
                task("root", vec![]),
                task("z", vec!["root"]),
                task("m", vec!["root"]),
                task("a", vec!["root"]),
            ],
        };
        let waves = config.into_waves().unwrap();
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[1][0].name, "z");
        assert_eq!(waves[1][1].name, "m");
        assert_eq!(waves[1][2].name, "a");
    }

    #[test]
    fn empty_name_error() {
        let config = TasksConfig { tasks: vec![task("", vec![])] };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn path_traversal_name_error() {
        let config = TasksConfig { tasks: vec![task("../evil", vec![])] };
        let err = config.into_waves().unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn sandbox_deserializes_from_yaml() {
        let yaml = r#"
tasks:
  - name: "x"
    cwd: "/tmp"
    prompt: "hello"
    sandbox: "read-write"
"#;
        let config: TasksConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.tasks[0].sandbox, Sandbox::ReadWrite);
    }

    #[test]
    fn sandbox_defaults_to_read_only() {
        let yaml = r#"
tasks:
  - name: "x"
    cwd: "/tmp"
    prompt: "hello"
"#;
        let config: TasksConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.tasks[0].sandbox, Sandbox::ReadOnly);
    }
}
