//! `kata add <template-spec> [--rev <ref>] [--at <dir>] [--var name=val]`
//!
//! Append a new template to this project's `applied.toml.templates`
//! and re-run `apply` so the new template's files land. Refuses if
//! the same `source` is already applied (use `kata update` to bump
//! the rev instead).

use camino::Utf8PathBuf;

use crate::ai::{agent_for_kind, resolve_backend};
use crate::applied::AppliedState;
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::manifest::AgentKind;
use crate::preset::TemplateRef;
use crate::runner::{PjApplyOptions, apply_to_pj};
use crate::ui;

use super::{parse_cli_vars, resolve_pj_root};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    template_spec: String,
    rev: Option<String>,
    at: Option<Utf8PathBuf>,
    vars: Vec<String>,
    ai_kind: AgentKind,
    no_ai: bool,
    yes: bool,
    ai_prompt: Option<String>,
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
    let base_dir = applied.base_dir.clone().unwrap_or(cwd);

    if applied.templates.iter().any(|t| t.source == template_spec) {
        return Err(Error::Config(format!(
            "template `{template_spec}` is already applied to this project; use `kata update` to bump its rev"
        )));
    }

    // Existing templates + the new one (last wins on file conflicts).
    let mut templates: Vec<TemplateRef> = applied
        .templates
        .iter()
        .map(|t| TemplateRef {
            source: t.source.clone(),
            rev: Some(t.rev.clone()),
            subdir: t.subdir.clone(),
        })
        .collect();
    templates.push(TemplateRef {
        source: template_spec.clone(),
        rev,
        subdir: None,
    });

    let project = ProjectEntry {
        name: pj_root.file_name().unwrap_or("kata-project").to_string(),
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    let agent = if no_ai { None } else { agent_for_kind(ai_kind) };
    let agent_backend = if no_ai {
        None
    } else {
        resolve_backend(ai_kind)
    };

    let opts = PjApplyOptions {
        dry_run: false,
        no_ai,
        interactive,
        cli_vars: parse_cli_vars(vars)?,
        // The new template's `when = "once"` files are not yet in
        // `applied.toml.files`, so the standard "once = fire if not
        // recorded" check picks them up. Forcing here would also
        // re-fire the *existing* templates' once-files.
        force_once: false,
        yes_all: yes,
        ai_prompt,
        agent_backend,
    };
    let result = apply_to_pj(
        project,
        pj_root.clone(),
        templates,
        base_dir,
        toml::Table::new(),
        applied.preset.clone(),
        opts,
        agent,
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
