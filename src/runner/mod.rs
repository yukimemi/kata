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
use crate::render::{Renderer, VarResolver, VarSources, build_context, deep_merge_table};
use crate::template::TemplateHandle;

#[derive(Debug, Clone)]
pub struct PjApplyOptions {
    pub dry_run: bool,
    pub no_ai: bool,
    pub interactive: bool,
    pub cli_vars: BTreeMap<String, toml::Value>,
    /// Force re-firing `when = "once"` files even when
    /// `applied.toml` already records them as `once_applied =
    /// true`. Currently always `false` for `init` / `apply` /
    /// `add` — once-files normally fire on the first run via the
    /// `once_applied` record being absent, and a pre-existing
    /// file on first run is treated as adoption (kept as-is).
    /// Reserved for a future `--force-once` flag that explicitly
    /// re-runs the template's body over the consumer's content.
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
    /// `--reseed <PATH>` (repeatable): dst paths whose
    /// `when = "once"` gate should be bypassed for this apply,
    /// re-emitting the template seed even when `applied.toml`
    /// records `once_applied = true`. The post-loop pass
    /// re-stamps `once_applied = true` for any reseeded entry
    /// that actually ran, leaving the persisted `applied.toml`
    /// identical to a fresh apply. Paths not declared in any
    /// active manifest are a silent no-op for that PJ.
    ///
    /// Stored as a `HashSet` so the per-entry membership check
    /// in `apply_to_pj` is O(1) and so `kata apply --all` only
    /// pays the `Vec → HashSet` conversion once at the CLI
    /// boundary instead of once per PJ.
    pub reseed: std::collections::HashSet<String>,
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

    // `--reseed <PATH>` (#82) is implemented as a per-entry gate
    // bypass: when `opts.reseed.contains(&state_key)` the
    // `when = "once"` block (skip / adoption / non-regular
    // refusal) is skipped entirely and the entry's mode runs as
    // if the file had never been applied. The post-loop pass
    // re-stamps `once_applied = true` because the entry hits
    // `once_applied_dsts` via the standard write path, so the
    // final persisted `applied.toml` is identical to a fresh
    // apply.
    //
    // Earlier drafts also flipped `applied.files[path].once_applied`
    // to false at this point, but that was a redundant footgun:
    // because the gate is bypassed via `opts.reseed.contains(...)`,
    // the in-memory flag is never consulted. Worse, if a reseeded
    // entry was later short-circuited by a `when_expr` evaluating
    // to false it would never reach the post-loop re-stamping,
    // and the cleared flag would be persisted to disk — losing
    // the consumer's recorded "once" state. Removed; see PR #89
    // discussion.

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

    // 3. Resolve vars (precedence: CLI > env > .kata/vars.toml >
    //    applied > preset > template seed > default > prompt). The
    //    prompter closure delegates to the interactive layer.
    //    The template seed is loaded from any `[[file]]` declaration
    //    targeting `.kata/vars.toml`, so a first-time consumer can
    //    render `.tera` files that reference `{{ vars.* }}` before
    //    the seed has actually been written to disk (#53).
    let env_vars = VarSources::from_env();
    let vars_file = VarSources::load_vars_file(&pj_root)?;
    let template_seed = collect_template_seed_vars(&handles)?;
    let sources = VarSources {
        cli: opts.cli_vars.clone(),
        env: env_vars,
        vars_file,
        applied: applied.vars.clone(),
        preset: preset_vars,
        template_seed,
    };
    let resolver = VarResolver {
        specs: &all_specs,
        sources: &sources,
        interactive: opts.interactive,
        prompter: |name: &str, spec: &VarSpec| crate::interactive::prompt_var(name, spec),
    };
    let resolved = resolver.resolve()?;
    let vars = &resolved.values;

    // 4. Build the rendering context once per PJ — Phase 1 has no
    //    per-file context overrides.
    let ctx = build_context(&project, &pj_root, vars);
    let mut renderer = Renderer::new();

    // 5. Walk templates × files in compose order.
    let mut actions = Vec::new();
    let mut errors = Vec::new();
    let mut applied_templates: Vec<AppliedTemplate> = Vec::new();
    let mut has_any_write = false;

    // Dst paths that should be marked `once_applied = true` at the
    // END of this apply run, deferred from the per-entry write site.
    // The deferral is what makes `when = "once"` compose across
    // multiple `[[file]]` entries targeting the same dst (e.g.
    // pj-base's overwrite-seed of `.kata/vars.toml` plus pj-rust's
    // merge-toml additions). If the flag were set mid-loop the
    // second entry's gate check would skip it. See #85.
    let mut once_applied_dsts: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Dst paths kata has already written (or unchanged-recorded) in
    // THIS apply run. Used to disambiguate the once-entry adoption
    // path: "file exists on disk" means "consumer brought it" only
    // when kata didn't write it earlier in the same run. Without
    // this guard, a second when=once entry to the same dst hits
    // adoption-when-file-exists instead of running its mode (e.g.
    // merge-toml), defeating composition. See #85.
    let mut wrote_in_run: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Disk content per dst at the start of this apply, captured the
    // first time we touch that dst. Used by the post-loop net-delta
    // pass (#81): when multiple `when="always"` entries target the
    // same dst (e.g. overwrite-then-merge-toml on `renri.toml`), each
    // layer writes intermediate bytes and individually reports
    // `wrote`, but if the composed final disk content equals the
    // initial content the user-visible diff is zero — both layers
    // should then report `unchanged`. `None` means "not on disk at
    // start"; `Some(_)` means we read this content before any layer
    // wrote in this run.
    let mut initial_disk_by_dst: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    // Indices into `actions` per dst, populated alongside each
    // `actions.push((dst_rel, outcome.kind))`. The post-loop pass
    // walks groups of size >= 2 (single-entry dsts trust their own
    // mode's report) and rewrites `Wrote` to `Unchanged` when the
    // group's net delta is zero. See #81.
    let mut action_indices_by_dst: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();

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

            // Snapshot disk state on first encounter with this dst —
            // before any layer can have written to it in this run.
            // Drives the post-loop net-delta pass (#81). Safe to read
            // even for dry-run; the post-loop pass just skips itself
            // there.
            //
            // `is_file()` guard: if the dst is a directory (or any
            // non-regular special file), `read_existing_text` would
            // surface a generic I/O error before the per-entry once
            // path can emit its more helpful "destination exists but
            // is not a regular file" message. Treat non-files as
            // "no snapshot, no collapse" and let downstream handling
            // produce the dedicated error.
            if !initial_disk_by_dst.contains_key(&state_key) {
                let initial = if dst_abs.is_file() {
                    read_existing_text(dst_abs.as_path())?
                } else {
                    None
                };
                initial_disk_by_dst.insert(state_key.clone(), initial);
            }

            // when = "once" gating. `--reseed <path>` bypasses the
            // whole once-gate block (skip-when-applied, adoption,
            // non-regular refusal) so the explicit intent of
            // "re-emit the template seed over whatever's on disk
            // for this path" is honoured. The post-loop pass still
            // re-stamps `once_applied = true` because the entry's
            // mode runs and lands in `once_applied_dsts`. See #82.
            if spec.when == WhenMode::Once
                && !opts.force_once
                && !opts.reseed.contains(&state_key)
            {
                if let Some(state) = applied.files.get(&state_key) {
                    if state.once_applied {
                        action_indices_by_dst
                            .entry(state_key.clone())
                            .or_default()
                            .push(actions.len());
                        actions.push((dst_rel, OutcomeKind::Skipped));
                        continue;
                    }
                }
                // Adoption flow: kata is being introduced into a project
                // that already has a file matching this once-entry.
                // Treat the on-disk content as the consumer's chosen
                // "once" — record content_hash cleared and move on
                // without overwriting. `content_hash` is explicitly
                // cleared (not just left at default) so a stale hash
                // from an older kata version that did record once
                // hashes can't sneak through and trip drift detection.
                //
                // The `once_applied = true` flag is deferred to the
                // post-loop pass so multiple entries targeting the same
                // dst (e.g. an adoption from one layer plus a
                // merge-toml from another) all run in this apply
                // before any of them locks out the next. See #85.
                //
                // Skip adoption when kata wrote this dst earlier in
                // the current run: the file exists because pj-base
                // just seeded it, not because the consumer pre-staged
                // it. Falling through lets the second entry's mode
                // run its actual logic (e.g. pj-rust's merge-toml
                // additions on top of pj-base's seed).
                if dst_abs.is_file() && !wrote_in_run.contains(&state_key) {
                    if !opts.dry_run {
                        let mut fs = applied.files.get(&state_key).cloned().unwrap_or_default();
                        fs.content_hash = None;
                        applied.record(&state_key, fs);
                        once_applied_dsts.insert(state_key.clone());
                        wrote_in_run.insert(state_key.clone());
                    }
                    action_indices_by_dst
                        .entry(state_key.clone())
                        .or_default()
                        .push(actions.len());
                    actions.push((dst_rel, OutcomeKind::Adopted));
                    continue;
                }
                // Destination exists but isn't a regular file (likely
                // a directory or non-regular special file). Refuse
                // rather than `once_applied`-marking it — adopting
                // would permanently mask an invalid template
                // destination shape. Only applies to pre-existing
                // non-regular files; if kata itself wrote the dst
                // earlier in this run it's already a regular file.
                if dst_abs.exists() && !wrote_in_run.contains(&state_key) {
                    let msg = format!("destination exists but is not a regular file: {dst_abs}");
                    errors.push((dst_rel.clone(), msg));
                    action_indices_by_dst
                        .entry(state_key.clone())
                        .or_default()
                        .push(actions.len());
                    actions.push((dst_rel, OutcomeKind::Failed));
                    continue;
                }
            }
            // when = "manual" is never auto-applied here — Phase 1
            // doesn't expose --file targeting yet.
            if spec.when == WhenMode::Manual {
                action_indices_by_dst
                    .entry(state_key.clone())
                    .or_default()
                    .push(actions.len());
                actions.push((dst_rel, OutcomeKind::Skipped));
                continue;
            }

            // when_expr predicate
            if let Some(expr) = &spec.when_expr {
                if !eval_truthy(&mut renderer, expr, &ctx)? {
                    action_indices_by_dst
                        .entry(state_key.clone())
                        .or_default()
                        .push(actions.len());
                    actions.push((dst_rel, OutcomeKind::Skipped));
                    continue;
                }
            }

            // Read source body and render.
            let raw = match std::fs::read_to_string(src_abs.as_std_path()) {
                Ok(s) => s,
                Err(e) => {
                    errors.push((dst_rel.clone(), format!("read source: {e}")));
                    action_indices_by_dst
                        .entry(state_key.clone())
                        .or_default()
                        .push(actions.len());
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
                vars,
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
                    action_indices_by_dst
                        .entry(state_key.clone())
                        .or_default()
                        .push(actions.len());
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
            // Also record unchanged files so applied.toml tracks every
            // file the template delivers (needed for `when = "once"`
            // guard and drift detection).
            if !opts.dry_run && matches!(outcome.kind, OutcomeKind::Wrote | OutcomeKind::Unchanged)
            {
                let is_once = matches!(spec.when, WhenMode::Once);
                let mut fs = applied.files.get(&state_key).cloned().unwrap_or_default();
                // `once` files are consumer-owned after the first
                // write — don't track their content_hash so `kata
                // status` doesn't emit drift noise on every later
                // consumer edit. The existing `Some(expected) else
                // continue` guard in `cmd::status::check_drift` then
                // skips them automatically. We also explicitly clear
                // any pre-existing hash on the once branch so a
                // forced re-apply (`force_once`) over a previously
                // hashed entry doesn't leave a stale value behind.
                if is_once {
                    fs.content_hash = None;
                } else {
                    fs.content_hash = Some(hash_content(action_ctx.rendered_body.as_bytes()));
                }
                applied.record(&state_key, fs);
                // Defer the `once_applied = true` flag to the post-loop
                // pass so multi-entry composition (e.g. layered
                // overwrite-then-merge to the same `.kata/vars.toml`)
                // isn't lock-out'd by a mid-loop flag set. See #85.
                if is_once {
                    once_applied_dsts.insert(state_key.clone());
                }
                // Mark this dst as kata-touched in the current run so
                // a later entry targeting the same dst doesn't fall
                // into the adoption path on the "file exists on disk"
                // check (the file exists because *we* just wrote it).
                wrote_in_run.insert(state_key.clone());

                // Track actual writes for applied_at
                if matches!(outcome.kind, OutcomeKind::Wrote) {
                    has_any_write = true;
                }
            }

            action_indices_by_dst
                .entry(state_key.clone())
                .or_default()
                .push(actions.len());
            actions.push((dst_rel, outcome.kind));
            // Drop: ActionContext borrowed `handle` and `spec`.
        }
    }

    // 6. Post-loop: stamp `once_applied = true` on every dst that any
    //    when=once entry wrote to (or adopted) during this apply.
    //    Deferred from mid-loop so that multiple entries targeting
    //    the same dst (cross-layer composition: overwrite-seed +
    //    merge-toml additions) all run before the flag locks them
    //    out. See #85.
    if !opts.dry_run {
        for dst in &once_applied_dsts {
            let mut fs = applied.files.get(dst).cloned().unwrap_or_default();
            fs.once_applied = true;
            applied.record(dst, fs);
        }
    }

    // 6b. Post-loop: collapse `Wrote` to `Unchanged` for dsts whose
    //     net delta across all layered entries is zero (#81). Each
    //     `when="always"` layer reports `Wrote` independently when
    //     its own byte-compare differs (overwrite clobbers the
    //     merged disk content; the next layer's merge-toml then
    //     restores it), but the user-visible effect is no change.
    //     Reading the dst once more here is cheap and avoids
    //     restructuring the per-mode `execute` contract.
    //
    //     Only runs for dsts with multiple `[[file]]` entries —
    //     single-entry dsts already report accurately from their
    //     mode's own byte-compare. Skipped under `--dry-run`
    //     because no layer actually wrote, and the per-mode plan
    //     already reflects the net-vs-disk comparison.
    if !opts.dry_run {
        for (dst_key, indices) in &action_indices_by_dst {
            if indices.len() < 2 {
                continue;
            }
            // Nothing to collapse if none of the entries actually
            // wrote — skip the disk re-read entirely. Common case
            // when both layers reported `Unchanged` already.
            let has_wrote = indices.iter().any(|&i| {
                actions
                    .get(i)
                    .is_some_and(|(_, k)| matches!(k, OutcomeKind::Wrote))
            });
            if !has_wrote {
                continue;
            }
            let dst_abs = pj_root.join(dst_key);
            // `is_file()` guard so a dst that ended up as a directory
            // (impossible via kata's own write path but cheap defence)
            // surfaces as "no final snapshot" rather than a generic
            // I/O error.
            let final_disk = if dst_abs.is_file() {
                read_existing_text(dst_abs.as_path())?
            } else {
                None
            };
            let initial = initial_disk_by_dst
                .get(dst_key)
                .cloned()
                .unwrap_or_default();
            if initial == final_disk {
                for &i in indices {
                    if let Some(slot) = actions.get_mut(i) {
                        if matches!(slot.1, OutcomeKind::Wrote) {
                            slot.1 = OutcomeKind::Unchanged;
                        }
                    }
                }
                // If every `Wrote` for this dst collapsed away, the
                // `applied_at` bookkeeping should also reflect "no
                // real write happened to this dst". `has_any_write`
                // stays true if any OTHER dst genuinely wrote; we
                // only need to clear it when this collapse removed
                // the last `Wrote` from the run. Cheaper to just
                // re-derive from the post-collapse actions.
            }
        }
        // Re-derive has_any_write so an all-collapsed run doesn't
        // bump `applied_at`. After collapse some `Wrote`s may have
        // become `Unchanged` — the final stamp should reflect the
        // observable-disk-write truth, not the pre-collapse intent.
        has_any_write = actions.iter().any(|(_, k)| matches!(k, OutcomeKind::Wrote));
    }

    // 7. Write back applied.toml on success (even partial — see
    //    resilience principle).
    if !opts.dry_run {
        applied.preset = preset_spec;
        // Persist the resolution base so future `kata apply` runs can
        // re-resolve relative template sources (`../pj-base`) without
        // depending on cwd at the time of re-apply.
        applied.base_dir = Some(base_dir);
        applied.templates = applied_templates;
        if has_any_write {
            applied.applied_at = Some(Timestamp::now());
        }
        // Only persist vars whose resolution source is user-typed
        // (CLI / env / prompt). Everything else already lives in a
        // tracked source the renderer can re-read on next apply — no
        // need to duplicate. See yukimemi/kata#58.
        applied.vars = resolved
            .values
            .iter()
            .filter(|(k, _)| {
                resolved
                    .sources
                    .get(k.as_str())
                    .copied()
                    .is_some_and(|s| s.should_persist_in_applied())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
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
    let vars_file = VarSources::load_vars_file(&pj_root)?;
    let template_seed = collect_template_seed_vars(&handles)?;
    let sources = VarSources {
        cli: cli_vars,
        env: env_vars,
        vars_file,
        applied: applied.vars.clone(),
        preset: preset_vars,
        template_seed,
    };
    let resolver = VarResolver {
        specs: &all_specs,
        sources: &sources,
        interactive,
        prompter: |name: &str, spec: &VarSpec| crate::interactive::prompt_var(name, spec),
    };
    let resolved = resolver.resolve()?;
    let vars = &resolved.values;
    let ctx = build_context(&project, &pj_root, vars);
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
                // First-time apply but the consumer already has the
                // file — `apply` will adopt it as-is. Mirror that in
                // the preview so `kata status` doesn't promise an
                // overwrite that won't happen. `is_file()` (not
                // `exists()`) so a directory at `dst` shows up as
                // `Diverged`, matching how the runner refuses to
                // adopt non-regular files.
                if dst_abs.is_file() {
                    out.push((dst_rel, crate::modes::PlanKind::AdoptedExisting, None));
                    continue;
                }
                if dst_abs.exists() {
                    out.push((dst_rel, crate::modes::PlanKind::Diverged, None));
                    continue;
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
                vars,
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
/// Auto-load every template's source `vars.toml` (if it ships one
/// via a `[[file]]` declaration targeting `.kata/vars.toml`) into a
/// merged seed table. Used as the lowest-priority "template-side"
/// var source so the renderer can see seeded values on the **first**
/// apply, before the seed has actually been written to the consumer's
/// `.kata/vars.toml` on disk. Without this, a fresh `kata apply`
/// against a template that uses `{{ vars.actions.checkout }}`-style
/// references would fail with "variable not found". See
/// yukimemi/kata#53.
///
/// Each template's seed is deep-merged in compose order — later
/// templates can extend (or override) earlier ones key-by-key, the
/// same way `[[file]]` overrides work elsewhere.
///
/// Only the **literal** destination `.kata/vars.toml` is recognised
/// (no Tera evaluation of `dst` yet — we don't have a context this
/// early). Templates that need a Tera-templated `dst` will silently
/// skip; that's the right behaviour for now.
fn collect_template_seed_vars(handles: &[TemplateHandle]) -> Result<toml::Table> {
    let mut seed = toml::Table::new();
    // Each layer ships its own `vars.toml` (#86: also `vars.<layer>.toml`).
    // We pull every `[[file]]` whose dst lands inside `.kata/` and
    // matches the vars-file naming rule (`vars.toml` or
    // `vars.<name>.toml`). Layered seeds compose across templates in
    // compose order, and within one template every `vars.<X>.toml`
    // alphabetically followed by `vars.toml` last — same
    // ordering rule the consumer-side `load_vars_file` uses
    // (see `VarSources::load_vars_file` doc).
    for handle in handles {
        let mut layer_specs: Vec<&crate::manifest::FileSpec> = handle
            .manifest
            .files
            .iter()
            .filter(|spec| spec_is_vars_seed(spec))
            .collect();
        // `vars.toml` goes last so its deep-merge wins on a
        // leaf-key conflict regardless of layer-name first letter.
        // Plain `sort_by_key` on the path can't enforce this on
        // its own (e.g. `vars.web.toml` would lex-sort after
        // `vars.toml`). See #93.
        layer_specs.sort_by(|a, b| {
            let dst_a = a.dst_or_src();
            let dst_b = b.dst_or_src();
            let is_bare_a = dst_a.ends_with("/vars.toml") || dst_a == "vars.toml";
            let is_bare_b = dst_b.ends_with("/vars.toml") || dst_b == "vars.toml";
            match (is_bare_a, is_bare_b) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => dst_a.cmp(dst_b),
            }
        });
        for spec in layer_specs {
            // Same security check as the apply loop — refuse
            // template-supplied paths that try to escape the
            // template root. `collect_template_seed_vars` runs
            // BEFORE the apply loop's check, so without this a
            // hostile / buggy manifest could read e.g.
            // `../../etc/passwd` via a `[[file]] src =
            // "../etc/passwd", dst = ".kata/vars.toml"` declaration.
            check_relative_contained(&spec.src, "template src")?;
            let src_abs = handle.root.join(&spec.src);
            let content = match std::fs::read_to_string(src_abs.as_std_path()) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::io_at(src_abs.as_std_path(), e)),
            };
            let parsed: toml::Table = toml::from_str(&content)
                .map_err(|e| Error::Config(format!("parse template seed `{src_abs}`: {e}")))?;
            deep_merge_table(&mut seed, parsed);
        }
    }
    Ok(seed)
}

/// True when a file-spec ships an entry that lands inside `.kata/`
/// with a name matching the vars-file pattern. Used by
/// [`collect_template_seed_vars`] to pull every per-layer seed.
/// Reuses `FileSpec::dst_or_src()` so the effective-dst logic
/// stays in one place (no parallel implementation to drift).
///
/// The dst string is normalised before the prefix check so that
/// `./.kata/vars.toml` or `foo/../.kata/vars.web.toml` are
/// recognised as vars seeds — both forms resolve into `.kata/`
/// at apply time anyway, and treating them inconsistently here
/// would make first-run template seeding differ from
/// subsequent runs (when kata reads the written file from disk).
/// See PR #93 review.
fn spec_is_vars_seed(spec: &crate::manifest::FileSpec) -> bool {
    use crate::render::vars::{KATA_DIR_REL, matches_vars_pattern};
    let normalized = normalize_relative_path(spec.dst_or_src());
    let prefix = format!("{KATA_DIR_REL}/");
    let Some(name) = normalized.strip_prefix(prefix.as_str()) else {
        return false;
    };
    // Reject any further sub-directory under `.kata/` (e.g.
    // `.kata/sub/vars.toml`) — kata's bookkeeping lives flat.
    if name.contains('/') {
        return false;
    }
    matches_vars_pattern(name)
}

/// Collapse `.` / `..` components in a relative path without
/// touching the filesystem. Mixed `/` and `\` separators both
/// fold into `/` for the comparison, so a template author can
/// write the dst in either convention.
///
/// Returns an empty string if the path tries to escape the root
/// (more `..` than non-`..` components). `spec_is_vars_seed` then
/// drops the entry because the empty string won't `strip_prefix(".kata/")`.
fn normalize_relative_path(s: &str) -> String {
    use std::path::{Component, Path, PathBuf};
    let unified: String = s.chars().map(|c| if c == '\\' { '/' } else { c }).collect();
    let mut buf = PathBuf::new();
    for c in Path::new(&unified).components() {
        match c {
            Component::CurDir => continue,
            Component::ParentDir => {
                if !buf.pop() {
                    // Escapes the root — caller's `strip_prefix(".kata/")`
                    // will fail, which is the right behaviour for
                    // `../etc/passwd`-style attempts.
                    return String::new();
                }
            }
            Component::Normal(seg) => buf.push(seg),
            Component::RootDir | Component::Prefix(_) => {
                // Absolute paths are rejected by the apply loop
                // anyway (`check_relative_contained`); returning
                // empty here means `spec_is_vars_seed` returns
                // false rather than matching against the unsafe
                // input.
                return String::new();
            }
        }
    }
    buf.to_string_lossy().replace('\\', "/")
}

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
