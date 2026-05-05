//! `kata unregister <name|path>` — drop a project from the global
//! registry. The PJ's `.kata/applied.toml` is left alone; this
//! only removes the registry pointer.

use crate::config::GlobalConfig;
use crate::error::Result;

pub fn run(key: String, no_color: bool) -> Result<()> {
    let _ = no_color;
    let mut config = GlobalConfig::load()?;
    config.remove_project(&key)?;
    config.save()?;
    println!("unregistered `{key}`");
    Ok(())
}
