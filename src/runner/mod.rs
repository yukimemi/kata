//! Phase 1 runner — synchronous, single-PJ orchestration.
//!
//! This is intentionally minimal: it walks the templates of one
//! project sequentially and applies each file via its `ApplyMode`.
//! The tokio fan-out / `JoinSet` / `Semaphore` machinery the
//! ROADMAP describes lands in Phase 3 when AI mode and multi-PJ
//! `--all` arrive.

use std::collections::BTreeMap;
use std::sync::Arc;

use camino::Utf8PathBuf;
use jiff::Timestamp;
use tokio::sync::Semaphore;

use crate::ai::{AiAgent, Backend};
use crate::applied::{AppliedState, AppliedTemplate};
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::manifest::{AiMode, FileSpec, VarSpec, WhenMode};
use crate::modes::{ActionContext, OutcomeKind, for_how};
use crate::preset::TemplateRef;
use crate::render::{Renderer, VarResolver, VarSources, build_context};
use crate::template::TemplateHandle;

#[derive(Debug, Clone)]
pub struct PjApplyOptions {
    pub dry_run: bool,
    pub no_ai: bool,
    pub interactive: bool,
    pub cli_vars: BTreeMap<String, toml::Value>,
    /// Treat `when = "once"` files as eligible for application even
    /// when applied state already has them — set by `kata init`,
    /// false by `kata apply` so once-files only fire on the first
    /// run.
    pub force_once: bool,
    /// `--yes`: accept AI-generated bodies non-interactively. The
    /// chezmoi-style per-file dialog (Phase 3-b3) flips this on
    /// per-file once the user picks `[a]ccept`.
    pub yes_all: bool,
    /// `--ai-prompt <msg>`: extra free-form instruction prepended
    /// to every `how = "ai"` request for this run (e.g. "respond
    /// in Japanese", "always keep my Section X"). `None` when not
    /// passed.
    pub ai_prompt: Option<String>,
    /// Backend the agent (if any) is using. Mirrored into the
    /// `ActionContext` so `[h]andoff` can spawn the CLI directly
    /// rather than going through the `AiAgent` trait.
    pub agent_backend: Option<Backend>,
    /// `--ai-mode <chat|handoff>`: run-wide override for the
    /// per-file `ai_mode`. `Some(Handoff)` forces every `how = "ai"`
    /// file to skip the chat loop and spawn the agent CLI
    /// interactively. `None` honours each manifest's `ai_mode`
    /// (default `Chat`).
    pub ai_mode_override: Option<AiMode>,
    /// Maximum concurrent AI calls (chat turns / handoff spawns /
    /// editor round-trips). Sourced from
    /// `defaults.ai_concurrency` (default 4) but overridable per
    /// run via `--ai-concurrency <N>`. Capped to >= 1 so a 0
    /// from a hand-edited config doesn't deadlock the apply.
    pub ai_concurrency: usize,
}

#[derive(Debug)]
pub struct PjApplyResult {
    pub project_name: String,
    /// Per-file outcome in the order the manifest declares them
    /// (across all templates, in compose order).
    pub actions: Vec<(String, OutcomeKind)>,
    pub errors: Vec<(String, String)>,
}

/// Apply a list of templates to a single project. The caller is
/// responsible for resolving the template refs (e.g. from a preset
/// or from the existing `applied.toml`) and for deciding whether
/// once-mode files should fire.
///
/// The argument list is wide on purpose for Phase 1 — Phase 3 will
/// fold these into an `ApplyPlan` once tokio fan-out lands and
/// `runner::execute(plan)` becomes the single entry point.
#[allow(clippy::too_many_arguments)]
pub async fn apply_to_pj(
    project: ProjectEntry,
    pj_root: Utf8PathBuf,
    templates: Vec<TemplateRef>,
    base_dir: Utf8PathBuf,
    preset_vars: toml::Table,
    preset_spec: Option<String>,
    opts: PjApplyOptions,
    agent: Option<Arc<dyn AiAgent>>,
) -> Result<PjApplyResult> {
    let mut applied = AppliedState::load(&pj_root)?;

    // Global AI gate. We always create one so `ActionContext` can
    // borrow it unconditionally; the cap is `opts.ai_concurrency`
    // (default 4 from `defaults.ai_concurrency`). A 0 from a hand-
    // edited config would deadlock acquire(), so floor at 1.
    let ai_sema = Arc::new(Semaphore::new(opts.ai_concurrency.max(1)));

    // 1. Load all template handles (so we can union their var specs
    //    before prompting).
    let mut handles: Vec<TemplateHandle> = Vec::with_capacity(templates.len());
    for t in &templates {
        handles.push(TemplateHandle::load(t, &base_dir).await?);
    }

    // 2. Union var specs across templates (later templates win on
    //    duplicate spec keys, matching the file-level last-wins).
    let mut all_specs: BTreeMap<String, VarSpec> = BTreeMap::new();
    for h in &handles {
        for (k, v) in &h.manifest.vars {
            all_specs.insert(k.clone(), v.clone());
        }
    }

    // 3. Resolve vars (precedence: CLI > env > applied > preset >
    //    default > prompt). The prompter closure delegates to the
    //    interactive layer.
    let env_vars = VarSources::from_env();
    let sources = VarSources {
        cli: opts.cli_vars.clone(),
        env: env_vars,
        applied: applied.vars.clone(),
        preset: preset_vars,
    };
    let resolver = VarResolver {
        specs: &all_specs,
        sources: &sources,
        interactive: opts.interactive,
        prompter: |name: &str, spec: &VarSpec| crate::interactive::prompt_var(name, spec),
    };
    let vars = resolver.resolve()?;

    // 4. Build the rendering context once per PJ — Phase 1 has no
    //    per-file context overrides.
    let ctx = build_context(&project, &pj_root, &vars);
    let mut renderer = Renderer::new();

    // 5. Walk templates × files in compose order.
    let mut actions = Vec::new();
    let mut errors = Vec::new();
    let mut applied_templates: Vec<AppliedTemplate> = Vec::new();

    for handle in &handles {
        applied_templates.push(AppliedTemplate {
            source: handle.source_spec.clone(),
            rev: handle.rev.clone(),
            subdir: handle.subdir.clone(),
            version: handle.manifest.version.clone(),
        });

        for spec in &handle.manifest.files {
            // Reject template-supplied paths that try to escape the
            // template root or PJ root via `..` / absolute paths.
            // (Critical security check — prevents template metadata
            // from turning apply into an arbitrary read/write.)
            check_relative_contained(&spec.src, "template src")?;
            let dst_rel = render_dst(&mut renderer, spec, &ctx)?;
            check_relative_contained(&dst_rel, "destination")?;
            let dst_abs = pj_root.join(&dst_rel);
            let src_abs = handle.root.join(&spec.src);

            let state_key = dst_rel.clone();

            // when = "once" gating
            if spec.when == WhenMode::Once && !opts.force_once {
                if let Some(state) = applied.files.get(&state_key) {
                    if state.once_applied {
                        actions.push((dst_rel, OutcomeKind::Skipped));
                        continue;
                    }
                }
            }
            // when = "manual" is never auto-applied here — Phase 1
            // doesn't expose --file targeting yet.
            if spec.when == WhenMode::Manual {
                actions.push((dst_rel, OutcomeKind::Skipped));
                continue;
            }

            // when_expr predicate
            if let Some(expr) = &spec.when_expr {
                if !eval_truthy(&mut renderer, expr, &ctx)? {
                    actions.push((dst_rel, OutcomeKind::Skipped));
                    continue;
                }
            }

            // Read source body and render.
            let raw = match std::fs::read_to_string(src_abs.as_std_path()) {
                Ok(s) => s,
                Err(e) => {
                    errors.push((dst_rel.clone(), format!("read source: {e}")));
                    actions.push((dst_rel, OutcomeKind::Failed));
                    continue;
                }
            };
            let rendered_body = render_or_passthrough(spec, raw, &ctx, &mut renderer)?;
            let current_body = read_existing_text(dst_abs.as_path())?;

            let mode = for_how(spec.how);
            let action_ctx = ActionContext {
                project: &project,
                pj_root: pj_root.as_path(),
                template: handle,
                spec,
                src_abs,
                dst_abs: dst_abs.clone(),
                rendered_body,
                current_body,
                vars: &vars,
                tera_ctx: &ctx,
                agent: agent.clone(),
                agent_backend: opts.agent_backend,
                interactive: opts.interactive,
                yes_all: opts.yes_all,
                ai_prompt: opts.ai_prompt.as_deref(),
                ai_mode_override: opts.ai_mode_override,
                ai_sema: ai_sema.clone(),
            };

            let outcome = match mode.execute(&action_ctx, opts.dry_run).await {
                Ok(o) => o,
                Err(e) => {
                    errors.push((dst_rel.clone(), e.to_string()));
                    actions.push((dst_rel, OutcomeKind::Failed));
                    continue;
                }
            };

            // A mode can also report failure as `Ok(ActionOutcome
            // { kind: Failed, error: Some(_) })` — typically when
            // the underlying error isn't a Rust `Result::Err` (a
            // child process exiting non-zero, an AI backend
            // returning a refusal, …). Surface those so the caller
            // can exit non-zero and the user can see what broke.
            if matches!(outcome.kind, OutcomeKind::Failed) {
                let msg = outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "failed (no error message)".to_string());
                errors.push((dst_rel.clone(), msg));
            }

            // Update applied state on success (skip when dry-run).
            if !opts.dry_run && matches!(outcome.kind, OutcomeKind::Wrote) {
                let once_applied = matches!(spec.when, WhenMode::Once);
                let mut fs = applied.files.get(&state_key).cloned().unwrap_or_default();
                fs.once_applied = fs.once_applied || once_applied;
                fs.content_hash = Some(hash_content(action_ctx.rendered_body.as_bytes()));
                applied.record(&state_key, fs);
            }

            actions.push((dst_rel, outcome.kind));
            // Drop: ActionContext borrowed `handle` and `spec`.
        }
    }

    // 6. Write back applied.toml on success (even partial — see
    //    resilience principle).
    if !opts.dry_run {
        applied.preset = preset_spec;
        // Persist the resolution base so future `kata apply` runs can
        // re-resolve relative template sources (`../pj-base`) without
        // depending on cwd at the time of re-apply.
        applied.base_dir = Some(base_dir);
        applied.templates = applied_templates;
        applied.applied_at = Some(Timestamp::now());
        applied.vars = vars;
        applied.save(&pj_root)?;
    }

    Ok(PjApplyResult {
        project_name: project.name.clone(),
        actions,
        errors,
    })
}

fn render_dst(
    renderer: &mut Renderer,
    spec: &crate::manifest::FileSpec,
    ctx: &tera::Context,
) -> Result<String> {
    let raw = spec.dst_or_src();
    if !raw.contains("{{") && !raw.contains("{%") {
        return Ok(raw.to_string());
    }
    renderer.render(raw, ctx)
}

fn eval_truthy(renderer: &mut Renderer, expr: &str, ctx: &tera::Context) -> Result<bool> {
    let wrapped = format!("{{% if {expr} %}}1{{% else %}}0{{% endif %}}");
    let out = renderer.render(&wrapped, ctx)?;
    Ok(out.trim() == "1")
}

/// SHA-256 of `b` as a lowercase hex string. Used both at apply
/// time (record on `FileState.content_hash`) and at status time
/// (compare against the on-disk bytes to detect drift). Public
/// so `cmd::status` can re-compute on demand without going
/// through the full apply pipeline.
pub fn hash_content(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    let bytes = h.finalize();
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Plan-only walk for `kata status` / `kata diff`. Returns the per-file
/// `PlanKind` without writing anything. Variable resolution still runs
/// (so a missing-required-without-default still errors).
pub async fn plan_pj(
    project: ProjectEntry,
    pj_root: Utf8PathBuf,
    templates: Vec<TemplateRef>,
    base_dir: Utf8PathBuf,
    preset_vars: toml::Table,
    interactive: bool,
    cli_vars: BTreeMap<String, toml::Value>,
) -> Result<Vec<(String, crate::modes::PlanKind, Option<String>)>> {
    let applied = AppliedState::load(&pj_root)?;

    // Plan never invokes the agent, but `ActionContext` requires a
    // semaphore. Build a single 1-permit sema for the whole plan
    // and clone the `Arc` per file rather than allocating one
    // semaphore per `[[file]]` entry.
    let plan_sema = Arc::new(Semaphore::new(1));

    let mut handles: Vec<TemplateHandle> = Vec::with_capacity(templates.len());
    for t in &templates {
        handles.push(TemplateHandle::load(t, &base_dir).await?);
    }
    let mut all_specs: BTreeMap<String, VarSpec> = BTreeMap::new();
    for h in &handles {
        for (k, v) in &h.manifest.vars {
            all_specs.insert(k.clone(), v.clone());
        }
    }
    let env_vars = VarSources::from_env();
    let sources = VarSources {
        cli: cli_vars,
        env: env_vars,
        applied: applied.vars.clone(),
        preset: preset_vars,
    };
    let resolver = VarResolver {
        specs: &all_specs,
        sources: &sources,
        interactive,
        prompter: |name: &str, spec: &VarSpec| crate::interactive::prompt_var(name, spec),
    };
    let vars = resolver.resolve()?;
    let ctx = build_context(&project, &pj_root, &vars);
    let mut renderer = Renderer::new();

    let mut out = Vec::new();
    for handle in &handles {
        for spec in &handle.manifest.files {
            check_relative_contained(&spec.src, "template src")?;
            let dst_rel = render_dst(&mut renderer, spec, &ctx)?;
            check_relative_contained(&dst_rel, "destination")?;
            let dst_abs = pj_root.join(&dst_rel);
            let src_abs = handle.root.join(&spec.src);

            // when handling
            if spec.when == WhenMode::Once {
                if let Some(s) = applied.files.get(&dst_rel) {
                    if s.once_applied {
                        out.push((dst_rel, crate::modes::PlanKind::SkippedOnce, None));
                        continue;
                    }
                }
            }
            if spec.when == WhenMode::Manual {
                out.push((dst_rel, crate::modes::PlanKind::SkippedWhen, None));
                continue;
            }
            if let Some(expr) = &spec.when_expr {
                if !eval_truthy(&mut renderer, expr, &ctx)? {
                    out.push((dst_rel, crate::modes::PlanKind::SkippedWhen, None));
                    continue;
                }
            }

            let raw = match std::fs::read_to_string(src_abs.as_std_path()) {
                Ok(s) => s,
                Err(_) => {
                    out.push((dst_rel, crate::modes::PlanKind::Diverged, None));
                    continue;
                }
            };
            let rendered_body = render_or_passthrough(spec, raw, &ctx, &mut renderer)?;
            let current_body = read_existing_text(dst_abs.as_path())?;

            let mode = for_how(spec.how);
            let action_ctx = ActionContext {
                project: &project,
                pj_root: pj_root.as_path(),
                template: handle,
                spec,
                src_abs,
                dst_abs: dst_abs.clone(),
                rendered_body,
                current_body,
                vars: &vars,
                tera_ctx: &ctx,
                agent: None,
                agent_backend: None,
                interactive,
                yes_all: false,
                ai_prompt: None,
                ai_mode_override: None,
                ai_sema: plan_sema.clone(),
            };
            let plan = mode.plan(&action_ctx).await?;
            out.push((dst_rel, plan.kind, plan.diff));
        }
    }
    Ok(out)
}

/// `.tera` opt-in render: when the spec opts in via the `.tera`
/// suffix on `src`, run the body through Tera; otherwise return
/// the raw text unchanged. "Unchanged" here means the UTF-8 source
/// text passes through verbatim — kata reads sources via
/// `std::fs::read_to_string`, so binary files aren't supported in
/// templates (tracked separately; not a Phase 1/2 concern).
///
/// Centralised so `apply_to_pj` and `plan_pj` cannot drift.
fn render_or_passthrough(
    spec: &FileSpec,
    raw: String,
    ctx: &tera::Context,
    renderer: &mut Renderer,
) -> Result<String> {
    if spec.is_tera_source() {
        renderer.render(&raw, ctx)
    } else {
        Ok(raw)
    }
}

/// Read a file's text, distinguishing "not present" from real I/O
/// errors. Returns `Ok(None)` only on `NotFound`; permission denied,
/// invalid UTF-8, etc. surface as errors so we don't silently
/// "create" over a file we just couldn't read.
fn read_existing_text(path: &camino::Utf8Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path.as_std_path()) {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io_at(path.as_std_path(), e)),
    }
}

/// Reject template-supplied relative paths that would escape their
/// root via `..` or be absolute. `kind` is for the error message
/// (`"template src"` / `"destination"`).
///
/// This is the load-bearing security check that makes apply safe
/// against hostile / buggy template metadata.
fn check_relative_contained(rel: &str, kind: &str) -> Result<()> {
    use std::path::{Component, Path};
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(Error::Other(anyhow::anyhow!(
            "{kind} path `{rel}` must be relative, not absolute"
        )));
    }
    let mut depth: i32 = 0;
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(Error::Other(anyhow::anyhow!(
                        "{kind} path `{rel}` escapes its root via `..`"
                    )));
                }
            }
            Component::Normal(_) => depth += 1,
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Other(anyhow::anyhow!(
                    "{kind} path `{rel}` must be relative"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_contained_accepts_simple_relative() {
        assert!(check_relative_contained("Makefile.toml", "x").is_ok());
        assert!(check_relative_contained("src/main.rs", "x").is_ok());
        assert!(check_relative_contained("a/b/c.txt", "x").is_ok());
        assert!(check_relative_contained("./Makefile.toml", "x").is_ok());
        assert!(check_relative_contained("a/./b", "x").is_ok());
        assert!(check_relative_contained("a/b/../c", "x").is_ok());
    }

    #[test]
    fn check_contained_rejects_traversal() {
        assert!(check_relative_contained("../etc/passwd", "x").is_err());
        assert!(check_relative_contained("a/../../escape", "x").is_err());
        assert!(check_relative_contained("./../bad", "x").is_err());
    }

    #[test]
    fn check_contained_rejects_absolute() {
        assert!(check_relative_contained("/etc/passwd", "x").is_err());
        if cfg!(windows) {
            assert!(check_relative_contained(r"C:\Windows\System32", "x").is_err());
        }
    }
}
