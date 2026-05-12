//! `kata apply [--at <dir>] [--all [--tag <t>]] [--dry-run] [--var name=val]`
//!
//! Re-apply this project's recorded templates. With `--all`, walks
//! every project in the global registry instead and runs apply
//! against each in parallel (gated by `defaults.pj_concurrency`).

use std::sync::Arc;

use camino::Utf8PathBuf;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::ai::{agent_for_kind, resolve_backend};
use crate::applied::AppliedState;
use crate::config::{GlobalConfig, ProjectEntry};
use crate::error::{Error, Result};
use crate::manifest::{AgentKind, AiMode};
use crate::preset::TemplateRef;
use crate::runner::{PjApplyOptions, PjApplyResult, apply_to_pj};
use crate::ui;

use super::{
    parse_cli_vars, resolve_ai_concurrency, resolve_pj_concurrency, resolve_pj_root,
    resolve_project_name, select_registered_projects,
};

/// Single-PJ entry point. `--at` (defaulting to cwd) picks the PJ.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    at: Option<Utf8PathBuf>,
    dry_run: bool,
    vars: Vec<String>,
    ai_kind: AgentKind,
    no_ai: bool,
    yes: bool,
    ai_prompt: Option<String>,
    ai_mode_override: Option<AiMode>,
    ai_concurrency_override: Option<usize>,
    interactive: bool,
    no_color: bool,
) -> Result<()> {
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;

    let project = ProjectEntry {
        name: resolve_project_name(&pj_root).await,
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    let opts_template = build_options(
        dry_run,
        vars,
        ai_kind,
        no_ai,
        yes,
        ai_prompt,
        ai_mode_override,
        ai_concurrency_override,
        interactive,
    )?;

    let result = apply_one(project, ai_kind, no_ai, opts_template, Some(cwd)).await?;

    print_pj_outcome(&result, pj_root.as_str(), no_color);
    if !result.errors.is_empty() {
        return Err(Error::Other(anyhow::anyhow!(
            "{} file(s) failed to apply",
            result.errors.len()
        )));
    }
    Ok(())
}

/// Multi-PJ entry point. Walks `GlobalConfig.projects` (filtered
/// by `tag_filter`), spawns each through `apply_one` on a tokio
/// `JoinSet` gated by `defaults.pj_concurrency`. AI concurrency
/// is shared across all PJs at the existing `ai_concurrency` cap
/// (so `--all` against 8 PJs with 4 ai_concurrency still only
/// keeps 4 agent CLIs in flight at a time).
#[allow(clippy::too_many_arguments)]
pub async fn run_all(
    tag_filter: Vec<String>,
    dry_run: bool,
    vars: Vec<String>,
    ai_kind: AgentKind,
    no_ai: bool,
    yes: bool,
    ai_prompt: Option<String>,
    ai_mode_override: Option<AiMode>,
    ai_concurrency_override: Option<usize>,
    pj_concurrency_override: Option<usize>,
    interactive: bool,
    no_color: bool,
    allow_dirty: bool,
    skip_dirty: bool,
) -> Result<()> {
    let config = GlobalConfig::load()?;
    let mut projects = select_registered_projects(&config, &tag_filter);
    if projects.is_empty() {
        if tag_filter.is_empty() {
            println!(
                "no projects registered yet — `kata register` from inside a kata-managed PJ to add one."
            );
        } else {
            println!("no registered projects matched all of: {tag_filter:?}");
        }
        return Ok(());
    }

    // Pre-flight VCS dirty check (#80). Default behaviour aborts
    // before any PJ is touched if any has uncommitted user work,
    // so kata-driven changes don't get mixed with WIP. `--skip-dirty`
    // drops dirty PJs silently; `--allow-dirty` proceeds anyway
    // (the historical behaviour before this gate).
    if !allow_dirty {
        let mut dirty: Vec<(String, Utf8PathBuf, Vec<String>)> = Vec::new();
        for entry in &projects {
            if let Some(files) = crate::vcs::dirty_files(&entry.path).await? {
                if !files.is_empty() {
                    dirty.push((entry.name.clone(), entry.path.clone(), files));
                }
            }
        }
        if !dirty.is_empty() {
            print_dirty_report(&dirty, no_color);
            if skip_dirty {
                let dirty_paths: std::collections::HashSet<Utf8PathBuf> =
                    dirty.iter().map(|(_, p, _)| p.clone()).collect();
                projects.retain(|p| !dirty_paths.contains(&p.path));
                eprintln!(
                    "skipping {} dirty PJ(s); proceeding with the rest.",
                    dirty.len()
                );
                if projects.is_empty() {
                    return Ok(());
                }
            } else {
                return Err(Error::Other(anyhow::anyhow!(
                    "{} PJ(s) have uncommitted work. Re-run with `--allow-dirty` to proceed \
                     or `--skip-dirty` to apply only to clean PJs.",
                    dirty.len()
                )));
            }
        }
    }

    let opts_template = build_options(
        dry_run,
        vars,
        ai_kind,
        no_ai,
        yes,
        ai_prompt,
        ai_mode_override,
        ai_concurrency_override,
        interactive,
    )?;

    let pj_concurrency = resolve_pj_concurrency(pj_concurrency_override);
    let sema = Arc::new(Semaphore::new(pj_concurrency.max(1)));

    let mut set = JoinSet::new();
    for entry in projects {
        let sema = sema.clone();
        let opts = opts_template.clone();
        set.spawn(async move {
            let _permit = sema.acquire_owned().await.expect("sema closed");
            let label = entry.name.clone();
            let path = entry.path.clone();
            let outcome = apply_one(entry, ai_kind, no_ai, opts, None).await;
            (label, path, outcome)
        });
    }

    let mut total_errors = 0usize;
    while let Some(joined) = set.join_next().await {
        let (label, path, result) = match joined {
            Ok(t) => t,
            Err(e) => {
                eprintln!("\n[panic] join task: {e}");
                total_errors += 1;
                continue;
            }
        };
        match result {
            Ok(r) => {
                print_pj_outcome(&r, path.as_str(), no_color);
                total_errors += r.errors.len();
            }
            Err(e) => {
                eprintln!("\n[error] {label} ({path}): {e}");
                total_errors += 1;
            }
        }
    }

    if total_errors > 0 {
        return Err(Error::Other(anyhow::anyhow!(
            "{total_errors} file(s) / project(s) failed across the registry"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_options(
    dry_run: bool,
    vars: Vec<String>,
    _ai_kind: AgentKind,
    no_ai: bool,
    yes: bool,
    ai_prompt: Option<String>,
    ai_mode_override: Option<AiMode>,
    ai_concurrency_override: Option<usize>,
    interactive: bool,
) -> Result<PjApplyOptions> {
    let ai_concurrency = resolve_ai_concurrency(ai_concurrency_override);
    Ok(PjApplyOptions {
        dry_run,
        no_ai,
        interactive,
        cli_vars: parse_cli_vars(vars)?,
        force_once: false,
        yes_all: yes,
        ai_prompt,
        // The agent_backend (resolved from ai_kind) is set
        // per-PJ inside apply_one — same kind across PJs but the
        // agent factory is consulted once each.
        agent_backend: None,
        ai_mode_override,
        ai_concurrency,
    })
}

/// Apply against a single registered or ad-hoc project. Loads
/// `applied.toml`, materialises the template list, resolves the
/// agent (when `--no-ai` is off), and delegates to
/// `runner::apply_to_pj`.
///
/// `default_base_dir` is the fallback for relative template
/// sources when `applied.toml.base_dir` is missing — the
/// single-PJ entry point passes the user's cwd; the multi-PJ
/// entry point passes `None`, which falls back to the project's
/// own root (the only sensible default when fanning out).
async fn apply_one(
    project: ProjectEntry,
    ai_kind: AgentKind,
    no_ai: bool,
    template_opts: PjApplyOptions,
    default_base_dir: Option<Utf8PathBuf>,
) -> Result<PjApplyResult> {
    let pj_root = project.path.clone();
    let applied = AppliedState::load(&pj_root)?;
    if applied.templates.is_empty() {
        return Err(Error::Config(format!(
            "{pj_root}: applied.toml has no templates recorded"
        )));
    }

    let templates: Vec<TemplateRef> = applied
        .templates
        .iter()
        .map(|t| TemplateRef {
            source: t.source.clone(),
            rev: Some(t.rev.clone()),
            subdir: t.subdir.clone(),
        })
        .collect();

    let base_dir = applied
        .base_dir
        .clone()
        .or(default_base_dir)
        .unwrap_or_else(|| pj_root.clone());

    let agent = if no_ai { None } else { agent_for_kind(ai_kind) };
    let agent_backend = if no_ai {
        None
    } else {
        resolve_backend(ai_kind)
    };

    let mut opts = template_opts;
    opts.agent_backend = agent_backend;

    apply_to_pj(
        project,
        pj_root,
        templates,
        base_dir,
        toml::Table::new(),
        applied.preset.clone(),
        opts,
        agent,
    )
    .await
}

fn print_pj_outcome(result: &PjApplyResult, path: &str, no_color: bool) {
    ui::print_pj_header(&result.project_name, path, no_color);
    for (dst, kind) in &result.actions {
        ui::print_outcome(dst, *kind, no_color);
    }
    if !result.errors.is_empty() {
        eprintln!("\nerrors in {}:", result.project_name);
        for (dst, msg) in &result.errors {
            eprintln!("  {dst}: {msg}");
        }
    }
}

/// Render the pre-flight dirty-PJ list (#80) to stderr — one PJ
/// per row, with the first few dirty paths inline so the user can
/// spot whether the WIP is plausibly safe to ignore (e.g. a stray
/// Cargo.lock bump kata won't touch anyway). `no_color` is honoured
/// to stay compatible with the existing UI conventions.
fn print_dirty_report(dirty: &[(String, Utf8PathBuf, Vec<String>)], _no_color: bool) {
    eprintln!("\ndirty PJ(s) — kata refuses to apply over uncommitted work:");
    for (name, path, files) in dirty {
        // Cap the inline preview at three paths so a PJ with a
        // huge WIP doesn't drown the table; the count is preserved
        // so the user knows how big the rest is.
        let preview: Vec<&str> = files.iter().take(3).map(String::as_str).collect();
        let extra = files.len().saturating_sub(preview.len());
        let inline = if extra == 0 {
            preview.join(", ")
        } else {
            format!("{}, +{} more", preview.join(", "), extra)
        };
        eprintln!("  {name} ({path}): {inline}");
    }
}
