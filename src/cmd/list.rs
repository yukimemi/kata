//! `kata list` — by default, show what governs the current PJ;
//! with `--all`, walk the global registry and emit a one-row-per-PJ
//! overview instead.

use camino::Utf8PathBuf;

use crate::applied::AppliedState;
use crate::config::GlobalConfig;
use crate::error::{Error, Result};

use super::resolve_pj_root;

pub fn run(at: Option<Utf8PathBuf>, all: bool, no_color: bool) -> Result<()> {
    if all {
        return run_all(no_color);
    }
    run_single(at, no_color)
}

fn run_single(at: Option<Utf8PathBuf>, _no_color: bool) -> Result<()> {
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

fn run_all(_no_color: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    if config.projects.is_empty() {
        println!(
            "no projects registered yet — `kata register` from inside a kata-managed PJ to add one."
        );
        return Ok(());
    }

    let rows: Vec<RegistryRow> = config
        .projects
        .iter()
        .map(RegistryRow::from_entry)
        .collect();

    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let path_w = rows.iter().map(|r| r.path.len()).max().unwrap_or(4).max(4);
    let preset_w = rows
        .iter()
        .map(|r| r.preset.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let templates_w = 9; // header label "TEMPLATES"
    let applied_w = rows
        .iter()
        .map(|r| r.applied_at.len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_w$}  {:<path_w$}  {:<preset_w$}  {:<templates_w$}  {:<applied_w$}  STATUS",
        "NAME",
        "PATH",
        "PRESET",
        "TEMPLATES",
        "APPLIED",
        name_w = name_w,
        path_w = path_w,
        preset_w = preset_w,
        templates_w = templates_w,
        applied_w = applied_w,
    );
    for r in &rows {
        println!(
            "{:<name_w$}  {:<path_w$}  {:<preset_w$}  {:<templates_w$}  {:<applied_w$}  {}",
            r.name,
            r.path,
            r.preset,
            r.templates,
            r.applied_at,
            r.status,
            name_w = name_w,
            path_w = path_w,
            preset_w = preset_w,
            templates_w = templates_w,
            applied_w = applied_w,
        );
    }
    Ok(())
}

struct RegistryRow {
    name: String,
    path: String,
    preset: String,
    templates: String,
    applied_at: String,
    status: String,
}

impl RegistryRow {
    fn from_entry(entry: &crate::config::ProjectEntry) -> Self {
        let path = entry.path.as_str().to_string();
        // A registered PJ whose directory has been moved is a real-
        // world condition; surface it in STATUS rather than abort
        // the whole listing.
        if !entry.path.exists() {
            return Self {
                name: entry.name.clone(),
                path,
                preset: "-".into(),
                templates: "-".into(),
                applied_at: "-".into(),
                status: "missing dir".into(),
            };
        }
        match AppliedState::load(&entry.path) {
            Ok(applied) if applied.templates.is_empty() => Self {
                name: entry.name.clone(),
                path,
                preset: "(none)".into(),
                templates: "0".into(),
                applied_at: "never".into(),
                status: "not init'd".into(),
            },
            Ok(applied) => {
                let preset = applied
                    .preset
                    .clone()
                    .unwrap_or_else(|| "(none)".to_string());
                let applied_at = applied
                    .applied_at
                    .map(|t| format!("{t}"))
                    .unwrap_or_else(|| "never".to_string());
                Self {
                    name: entry.name.clone(),
                    path,
                    preset,
                    templates: applied.templates.len().to_string(),
                    applied_at,
                    status: "ok".into(),
                }
            }
            Err(e) => Self {
                name: entry.name.clone(),
                path,
                preset: "-".into(),
                templates: "-".into(),
                applied_at: "-".into(),
                status: format!("error: {e}"),
            },
        }
    }
}
