//! `kata list [--at <dir>]` — show which templates / files this PJ
//! is governed by.

use camino::Utf8PathBuf;

use crate::applied::AppliedState;
use crate::error::{Error, Result};

use super::resolve_pj_root;

pub fn run(at: Option<Utf8PathBuf>, _no_color: bool) -> Result<()> {
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;
    let applied = AppliedState::load(&pj_root)?;

    println!("project: {pj_root}");
    if let Some(p) = &applied.preset {
        println!("preset:  {p}");
    }
    println!();
    println!("templates ({} applied):", applied.templates.len());
    for t in &applied.templates {
        let v = t.version.as_deref().unwrap_or("-");
        println!("  - {} @ {} (manifest version: {})", t.source, t.rev, v);
    }
    println!();
    println!("vars:");
    for (k, v) in &applied.vars {
        println!("  {k} = {v}");
    }
    println!();
    println!("files ({} tracked):", applied.files.len());
    for (k, fs) in &applied.files {
        let once = if fs.once_applied { " (once)" } else { "" };
        println!("  - {k}{once}");
    }
    Ok(())
}
