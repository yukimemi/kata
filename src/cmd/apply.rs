//! `kata apply [--at <dir>] [--dry-run] [--var name=val]`
//!
//! Re-apply this project's recorded templates. Reads
//! `.kata/applied.toml` to know what to apply.

use camino::Utf8PathBuf;

use crate::applied::AppliedState;
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::preset::TemplateRef;
use crate::runner::{PjApplyOptions, apply_to_pj};
use crate::ui;

use super::{parse_cli_vars, resolve_pj_root};

pub async fn run(
    at: Option<Utf8PathBuf>,
    dry_run: bool,
    vars: Vec<String>,
    interactive: bool,
    no_color: bool,
) -> Result<()> {
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;

    let applied = AppliedState::load(&pj_root)?;
    if applied.templates.is_empty() {
        return Err(Error::Config(format!(
            "{pj_root}: applied.toml has no templates recorded"
        )));
    }

    // Convert AppliedTemplate back to TemplateRef. Phase 1 stores
    // local source paths verbatim, so this round-trips cleanly.
    let templates: Vec<TemplateRef> = applied
        .templates
        .iter()
        .map(|t| TemplateRef {
            source: t.source.clone(),
            rev: Some(t.rev.clone()),
            subdir: None,
        })
        .collect();

    let project = ProjectEntry {
        name: pj_root.file_name().unwrap_or("kata-project").to_string(),
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    let opts = PjApplyOptions {
        dry_run,
        no_ai: true,
        interactive,
        cli_vars: parse_cli_vars(vars)?,
        force_once: false,
    };

    let result = apply_to_pj(
        project,
        pj_root.clone(),
        templates,
        // For apply, the base_dir is irrelevant for already-absolute
        // local sources; pass cwd for the rare relative-source case.
        cwd,
        toml::Table::new(),
        applied.preset.clone(),
        opts,
        None,
    )
    .await?;

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
