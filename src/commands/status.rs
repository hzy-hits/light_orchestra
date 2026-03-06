use crate::dashboard;
use anyhow::Result;
use std::path::Path;

pub async fn status_command(log_dir: &Path, watch: Option<u64>) -> Result<()> {
    if let Some(interval) = watch {
        dashboard::watch(log_dir, interval).await?;
    } else {
        dashboard::render_once(log_dir)?;
    }
    Ok(())
}
