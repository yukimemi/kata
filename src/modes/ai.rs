//! `how = "ai"` — delegate the merge to an external agent CLI
//! (claude / gemini / codex / opencode) via the `AiAgent` trait.
//!
//! Phase 3-b2 ships the *non-interactive* slice of the spec: the
//! mode runs the agent only when the user opted in (`--yes`) and
//! the runner has resolved an agent. Every other path — no agent
//! on PATH, `--no-ai`, plain interactive run without `--yes` —
//! falls through to a clean `Skipped` outcome with
//! `Decision::Defer`. The chezmoi-style
//! `[a]ccept / [e]dit / [c]hat / [h]andoff / [s]kip / [d]efer`
//! dialog lands with Phase 3-b3.

use async_trait::async_trait;

use crate::ai::{AiRequest, DEFAULT_SYSTEM_PROMPT};
use crate::applied::Decision;
use crate::error::{Error, Result};

use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

/// Default per-chat-turn timeout. Mirrors the env-overridable
/// `KATA_AI_TIMEOUT_SECS` consumed by `ai::process::invoke_chat`.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

pub struct Ai;

#[async_trait]
impl ApplyMode for Ai {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan> {
        // We can't preview the merged body until the agent runs,
        // so the plan only reports whether we'd be creating or
        // updating. The real diff lands in `execute` once the
        // agent produces a body.
        let kind = match &ctx.current_body {
            None => PlanKind::Create,
            Some(_) => PlanKind::Update,
        };
        Ok(ActionPlan { kind, diff: None })
    }

    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome> {
        // Dry run: announce what we'd do but skip the (expensive)
        // agent round-trip.
        if dry_run {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Skipped,
                decision: Some(Decision::Defer),
                diff: None,
                error: None,
            });
        }

        // Non-interactive defaults to skip ("safe") per CLAUDE.md.
        // `--yes` flips that — only then do we actually call the
        // agent. The interactive `[a]ccept / [c]hat / [h]andoff /
        // …` UI is Phase 3-b3.
        if !ctx.yes_all {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Skipped,
                decision: Some(Decision::Defer),
                diff: None,
                error: None,
            });
        }

        // No agent → skip with a clear (non-fatal) note. Reasons
        // include `--no-ai`, the `auto` resolver finding nothing
        // on PATH, or an explicit `agent = "claude"` whose CLI
        // isn't installed (which the runner detects and drops).
        let Some(agent) = ctx.agent.clone() else {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Skipped,
                decision: Some(Decision::Defer),
                diff: None,
                error: Some(
                    "no AI agent available (try `--ai claude` / install one of \
                     claude / codex / gemini, or pass `--no-ai`)"
                        .into(),
                ),
            });
        };

        // The manifest's `prompt` field is the user-author guidance
        // for this file; it was already expanded through Tera at
        // template-load time so we can pass it straight through.
        let user_prompt = ctx.spec.prompt.clone().unwrap_or_default();

        let req = AiRequest {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            user_prompt,
            current: ctx.current_body.clone(),
            incoming: ctx.rendered_body.clone(),
            template_diff: None,
            dst: ctx.dst_abs.clone(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        };

        let response = agent.run(req).await?;
        let body = match response.full_body {
            Some(b) => b,
            None => {
                return Ok(ActionOutcome {
                    kind: OutcomeKind::Failed,
                    decision: None,
                    diff: None,
                    error: Some(
                        "AI response did not include a <kata:body>...</kata:body> block; \
                         leaving destination untouched"
                            .into(),
                    ),
                });
            }
        };

        // No-op when the agent's output already matches what's on
        // disk — no point in writing an identical body and
        // generating a useless `OutcomeKind::Wrote` line.
        if ctx.current_body.as_deref() == Some(body.as_str()) {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Unchanged,
                decision: Some(Decision::Accept),
                diff: None,
                error: None,
            });
        }

        let diff = unified_diff(
            ctx.current_body.as_deref().unwrap_or(""),
            &body,
            ctx.dst_abs.as_str(),
        );

        if let Some(parent) = ctx.dst_abs.parent() {
            tokio::fs::create_dir_all(parent.as_std_path())
                .await
                .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
        }
        tokio::fs::write(ctx.dst_abs.as_std_path(), &body)
            .await
            .map_err(|e| Error::io_at(ctx.dst_abs.as_std_path(), e))?;

        Ok(ActionOutcome {
            kind: OutcomeKind::Wrote,
            decision: Some(Decision::Accept),
            diff: Some(diff),
            error: None,
        })
    }
}
