//! `how = "script"` — execute a child process and report its
//! exit status. Useful for `npm install` / `bundle install` /
//! per-PJ post-init steps that don't lend themselves to a static
//! file copy.
//!
//! Phase 2-f minimum: spawn `spec.run.command` with Tera-rendered
//! args, cwd'd into the project root. Outcome is `Wrote` on
//! exit-zero, `Failed` otherwise. Per-script idempotency
//! (`when_run = once / onchange / every`) is a separate Phase 4
//! concern.

use std::path::PathBuf;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::{Error, Result};
use crate::manifest::ScriptSpec;
use crate::render::Renderer;

use super::{ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind};

pub struct Script;

#[async_trait]
impl ApplyMode for Script {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan> {
        let run = require_run(ctx)?;
        let (cmd, args) = render_run(ctx, run)?;
        Ok(ActionPlan {
            kind: PlanKind::Update,
            diff: Some(would_run_line(&cmd, &args)),
        })
    }

    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome> {
        let run = require_run(ctx)?;
        let (cmd, args) = render_run(ctx, run)?;

        if dry_run {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Skipped,
                decision: None,
                diff: Some(would_run_line(&cmd, &args)),
                error: None,
            });
        }

        let output = Command::new(&cmd)
            .args(&args)
            .current_dir(ctx.pj_root.as_std_path())
            .output()
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("spawn script `{cmd}`: {e}")))?;

        if !output.status.success() {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Failed,
                decision: None,
                diff: None,
                error: Some(format!(
                    "`{cmd}` exit {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
            });
        }

        Ok(ActionOutcome {
            kind: OutcomeKind::Wrote,
            decision: None,
            diff: None,
            error: None,
        })
    }
}

/// Render the `command` and every `args` element through Tera with
/// the standard context plus the `script_*` helpers (mirrored from
/// spyrun's hook helpers). Shared between `plan` and `execute` so
/// the previewed command and the actually-spawned command are
/// guaranteed to match.
fn render_run(ctx: &ActionContext<'_>, run: &ScriptSpec) -> Result<(String, Vec<String>)> {
    let mut local_ctx = ctx.tera_ctx.clone();
    local_ctx.insert("script_path", ctx.src_abs.as_str());
    local_ctx.insert(
        "script_dir",
        ctx.src_abs.parent().map(|p| p.as_str()).unwrap_or(""),
    );
    local_ctx.insert("script_name", ctx.src_abs.file_name().unwrap_or(""));
    local_ctx.insert("script_stem", ctx.src_abs.file_stem().unwrap_or(""));
    local_ctx.insert("script_ext", ctx.src_abs.extension().unwrap_or(""));

    let mut renderer = Renderer::new();
    let cmd = renderer.render(&run.command, &local_ctx)?;
    let mut args = Vec::with_capacity(run.args.len());
    for arg in &run.args {
        args.push(renderer.render(arg, &local_ctx)?);
    }
    Ok((cmd, args))
}

fn would_run_line(cmd: &str, args: &[String]) -> String {
    if args.is_empty() {
        format!("(would run `{cmd}`)")
    } else {
        format!("(would run `{cmd} {}`)", args.join(" "))
    }
}

fn require_run<'a>(ctx: &'a ActionContext<'_>) -> Result<&'a ScriptSpec> {
    ctx.spec.run.as_ref().ok_or_else(|| {
        Error::manifest(
            PathBuf::from(&ctx.template.source_spec),
            format!(
                "how=\"script\" requires a `run` table in `[[file]]` for {}",
                ctx.spec.src
            ),
        )
    })
}
