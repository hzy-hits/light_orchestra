use anyhow::Result;
use std::path::Path;

pub fn clean_command(base: &Path) -> Result<()> {
    std::fs::remove_dir_all(base.join("outputs")).ok();
    std::fs::remove_dir_all(base.join("logs")).ok();
    println!("Cleaned outputs/ and logs/");
    Ok(())
}
