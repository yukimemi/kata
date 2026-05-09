//! `kata init <preset> [--at <dir>] [--var name=val]`
//!
//! Bootstrap a project from a preset. Both **local** preset paths
//! (`./...` / absolute) and **git** preset specs
//! (`github.com/yukimemi/pj-presets:rust-cli`) are supported via
//! `Preset::resolve`.

use camino::Utf8PathBuf;

use crate::ai::{agent_for_kind, resolve_backend};
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::manifest::{AgentKind, AiMode};
use crate::preset::{Preset, PresetSpec};
use crate::runner::{PjApplyOptions, apply_to_pj};
use crate::template::TemplateCache;
use crate::ui;

use super::{
    ensure_state_dir, parse_cli_vars, resolve_ai_concurrency, resolve_pj_root, resolve_project_name,
};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    preset_spec: String,
    at: Option<Utf8PathBuf>,
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

    // 1. Parse the spec and resolve to (preset, base_dir). Local
    //    preset paths read straight off disk; remote git specs
    //    clone-on-first-use into the template cache (same slot
    //    infrastructure as `TemplateRef`'s git source).
    let spec = PresetSpec::parse(&preset_spec)?;
    let cache = TemplateCache::ensure()?;
    let (preset, base_dir) = Preset::resolve(&spec, &cache).await?;

    // 2. Build a synthetic ProjectEntry (we don't auto-register
    //    yet — registry handling is Phase 2-g).
    let project = ProjectEntry {
        name: resolve_project_name(&pj_root).await,
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    // 3. Apply.
    let agent = if no_ai { None } else { agent_for_kind(ai_kind) };
    let agent_backend = if no_ai {
        None
    } else {
        resolve_backend(ai_kind)
    };
    let ai_concurrency = resolve_ai_concurrency(ai_concurrency_override);
    let opts = PjApplyOptions {
        dry_run: false,
        no_ai,
        interactive,
        cli_vars: parse_cli_vars(vars)?,
        // `init` is the first apply against this PJ — once-files
        // fire naturally because they're not yet recorded as
        // `once_applied = true`. Forcing here would only matter
        // when re-initialising over an existing `applied.toml`,
        // and in that case we want to honour the consumer's
        // existing once content (adoption flow) rather than
        // overwrite it.
        force_once: false,
        yes_all: yes,
        ai_prompt,
        agent_backend,
        ai_mode_override,
        ai_concurrency,
    };
    let result = apply_to_pj(
        project,
        pj_root.clone(),
        preset.templates.clone(),
        base_dir,
        preset.vars.clone(),
        Some(preset_spec),
        opts,
        agent,
    )
    .await?;

    // 4. Print outcome.
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
