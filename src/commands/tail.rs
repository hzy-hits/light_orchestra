use crate::event::CodexEvent;
use crate::meta;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Tail with follow: reads existing lines, then polls for new ones until task completes.
pub async fn tail_follow(log_path: &PathBuf, meta_path: &PathBuf) -> anyhow::Result<()> {
    if !log_path.exists() {
        anyhow::bail!("Log file not found: {}", log_path.display());
    }

    let mut file = tokio::fs::File::open(log_path).await?;
    let mut reader = BufReader::new(&mut file);

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // EOF — check if task is still running
            if meta_path.exists() {
                if let Ok(meta) = meta::TaskMeta::load(meta_path) {
                    if meta.status.is_terminal() {
                        println!("\n--- Task {} ({}) ---", meta.name, meta.status);
                        break;
                    }
                }
            }
            // Not done yet, wait and retry
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }

        let line = line.trim_end();
        if let Some(evt) = CodexEvent::parse(line) {
            let ts = CodexEvent::extract_timestamp(line);
            match evt {
                CodexEvent::ReadFile { file_path } => {
                    println!("[{}] READ  {}", ts, file_path);
                }
                CodexEvent::ExecCommand { command } => {
                    println!("[{}] EXEC  {}", ts, command);
                }
                CodexEvent::TokenCount { input_tokens, .. } => {
                    println!("[{}] TOKENS {:.0}K", ts, input_tokens as f64 / 1000.0);
                }
                CodexEvent::TaskComplete => {
                    println!("[{}] === TASK COMPLETE ===", ts);
                }
                CodexEvent::Other(_) => {}
            }
        }
    }
    Ok(())
}
