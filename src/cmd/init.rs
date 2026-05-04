//! `kata init <preset> [--at <dir>] [--var name=val]`
//!
//! Bootstrap a project from a local preset. Phase 1 = local sources
//! only; remote (`github.com/...`) errors out clearly.

use std::path::PathBuf;

use camino::Utf8PathBuf;

use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::preset::{Preset, PresetSpec};
use crate::runner::{PjApplyOptions, apply_to_pj};
use crate::ui;

use super::{ensure_state_dir, parse_cli_vars, resolve_pj_root};

pub async fn run(
    preset_spec: String,
    at: Option<Utf8PathBuf>,
    vars: Vec<String>,
    interactive: bool,
    no_color: bool,
) -> Result<()> {
    let pj_root = resolve_pj_root(at)?;
    std::fs::create_dir_all(pj_root.as_std_path())
        .map_err(|e| Error::io_at(pj_root.as_std_path(), e))?;

    // Refuse to bootstrap inside a project that already has its own
    // `.kata/applied.toml` ancestor (Q9 in ROADMAP). Run *before*
    // ensure_state_dir so a refused init doesn't leave an orphan
    // `.kata/` behind.
    if let Some(existing) = crate::paths::find_pj_root(&pj_root) {
        if existing != pj_root {
            return Err(Error::Config(format!(
                "refusing to init: ancestor {existing} already has a kata project"
            )));
        }
    }
    ensure_state_dir(&pj_root)?;

    // 1. Parse and resolve the preset spec (Phase 1: local only).
    let spec = PresetSpec::parse(&preset_spec)?;
    if !spec.is_local() {
        return Err(Error::Preset {
            path: PathBuf::from(&preset_spec),
            message: "Phase 1 supports local presets only (use `./...` or an absolute path)".into(),
        });
    }
    let preset = Preset::resolve_local(&spec)?;

    // 2. Determine `base_dir` for resolving relative template
    //    sources inside the preset. It's the directory the preset
    //    file lives in.
    let base_dir = preset_base_dir(&preset_spec, &spec)?;

    // 3. Build a synthetic ProjectEntry (we don't auto-register in
    //    Phase 1).
    let project = ProjectEntry {
        name: pj_root.file_name().unwrap_or("kata-project").to_string(),
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    // 4. Apply.
    let opts = PjApplyOptions {
        dry_run: false,
        no_ai: true, // Phase 1: no AI yet
        interactive,
        cli_vars: parse_cli_vars(vars)?,
        force_once: true, // init runs once-files
    };
    let result = apply_to_pj(
        project,
        pj_root.clone(),
        preset.templates.clone(),
        base_dir,
        preset.vars.clone(),
        Some(preset_spec),
        opts,
        None,
    )
    .await?;

    // 5. Print outcome.
    ui::print_pj_header(&result.project_name, pj_root.as_str(), no_color);
    for (dst, kind) in &result.actions {
        ui::print_outcome(dst, *kind, no_color);
    }
    if !result.errors.is_empty() {
        eprintln!("\nerrors:");
        for (dst, msg) in &result.errors {
            eprintln!("  {dst}: {msg}");
        }
        return Err(Error::Other(anyhow::anyhow!(
            "{} file(s) failed to apply",
            result.errors.len()
        )));
    }
    Ok(())
}

fn preset_base_dir(spec_str: &str, spec: &PresetSpec) -> Result<Utf8PathBuf> {
    let path = Utf8PathBuf::from(&spec.source);
    if path.is_file() {
        return Ok(path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Utf8PathBuf::from(".")));
    }
    if path.is_dir() {
        return Ok(path);
    }
    Err(Error::Preset {
        path: spec_str.into(),
        message: format!("local preset source `{}` does not exist", spec.source),
    })
}
