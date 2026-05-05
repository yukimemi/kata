//! `kata register [<path>] [--name N] [--tags ...]` — record a
//! kata-managed project in the global registry
//! (`~/.config/kata/config.toml`).
//!
//! The registry is a *pointer* layer: kata never reads templates
//! through it (that's `applied.toml`'s job per-PJ), but it gives
//! `kata list --all` / `kata apply --all` a list of PJs to walk
//! when the user wants a multi-PJ overview.

use camino::Utf8PathBuf;

use crate::config::{GlobalConfig, ProjectEntry};
use crate::error::{Error, Result};

use super::{resolve_pj_root, resolve_project_name};

/// Add the PJ at `path` (or cwd) to the global registry. Refuses
/// to register a PJ that hasn't been `kata init`-ed yet — without
/// `.kata/applied.toml` there's nothing useful for the registry
/// to point at.
pub async fn run(
    path: Option<Utf8PathBuf>,
    name: Option<String>,
    tags: Vec<String>,
    no_color: bool,
) -> Result<()> {
    let _ = no_color;
    let cwd = resolve_pj_root(path)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; \
             run `kata init <preset>` first to make this a kata project."
        ))
    })?;

    let resolved_name = match name {
        Some(n) if !n.trim().is_empty() => n,
        _ => resolve_project_name(&pj_root).await,
    };

    let entry = ProjectEntry {
        name: resolved_name.clone(),
        path: pj_root.clone(),
        tags,
        overrides: None,
    };

    let mut config = GlobalConfig::load()?;
    config.add_project(entry)?;
    config.save()?;

    println!("registered `{resolved_name}` -> {pj_root}");
    Ok(())
}
