//! `kata status [--at <dir>]` — preview what `kata apply` would do.

use camino::Utf8PathBuf;

use crate::applied::AppliedState;
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::preset::TemplateRef;
use crate::runner::plan_pj;
use crate::ui;

use super::resolve_pj_root;

pub async fn run(at: Option<Utf8PathBuf>, interactive: bool, no_color: bool) -> Result<()> {
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;

    let applied = AppliedState::load(&pj_root)?;
    let templates: Vec<TemplateRef> = applied
        .templates
        .iter()
        .map(|t| TemplateRef {
            source: t.source.clone(),
            rev: Some(t.rev.clone()),
            subdir: t.subdir.clone(),
        })
        .collect();

    let project = ProjectEntry {
        name: pj_root.file_name().unwrap_or("kata-project").to_string(),
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    // Same fix as cmd/apply: prefer the recorded base_dir over cwd
    // so relative template sources resolve correctly.
    let base_dir = applied.base_dir.clone().unwrap_or_else(|| cwd.clone());

    let plans = plan_pj(
        project,
        pj_root.clone(),
        templates,
        base_dir,
        toml::Table::new(),
        interactive,
        Default::default(),
    )
    .await?;

    ui::print_pj_header(
        pj_root.file_name().unwrap_or("project"),
        pj_root.as_str(),
        no_color,
    );
    for (dst, kind, _diff) in &plans {
        ui::print_plan(dst, *kind, no_color);
    }
    Ok(())
}
