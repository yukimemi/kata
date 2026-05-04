//! `how = "ai"` — delegate the merge to an external agent CLI
//! (claude / gemini / codex / opencode) via the `AiAgent` trait.
//!
//! Decision matrix (Phase 3-b3):
//!
//! - `dry_run`              → Skipped + Defer  (no agent round-trip)
//! - no agent on PATH       → Skipped + Defer + stderr hint
//! - non-interactive
//!   - `--yes`              → run agent, accept the body verbatim
//!   - no `--yes`           → Skipped + Defer (CI-safe default)
//! - interactive
//!   - run agent, show the diff, prompt with `interactive::prompt_ai_decision`:
//!     - `[a]ccept`         → write the body
//!     - `[c]hat <instr>`   → re-run with the instruction appended
//!       and the prior proposal carried forward in the prompt history
//!     - `[s]kip`           → Skipped + Skip
//!     - `[d]efer`          → Skipped + Defer
//!
//! `[e]dit` (open `$EDITOR` on the AI body) and `[h]andoff` (spawn
//! the agent CLI interactively, kata stops re-importing) land with
//! Phase 3-b4.

use async_trait::async_trait;

use crate::ai::{AiRequest, DEFAULT_SYSTEM_PROMPT};
use crate::applied::Decision;
use crate::error::{Error, Result};
use crate::interactive::{AiDecision, prompt_ai_decision};

use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

/// Cap on how many `[c]hat` refinements we run before forcing the
/// user back to the top-level decision. Without a ceiling a noisy
/// session could keep spinning the agent forever.
const MAX_CHAT_TURNS: usize = 8;

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
            return Ok(skipped(Decision::Defer, None));
        }

        // CI / scripted callers: `--yes` opts in to accept the
        // first body verbatim; without it (and no TTY) we always
        // skip with `Defer` so the next interactive run picks it
        // back up.
        if !ctx.interactive && !ctx.yes_all {
            return Ok(skipped(Decision::Defer, None));
        }

        let Some(agent) = ctx.agent.clone() else {
            const HINT: &str = "no AI agent available (try `--ai claude` / install one of \
                claude / codex / gemini, or pass `--no-ai`)";
            eprintln!("  ai skip {}: {HINT}", ctx.dst_abs);
            return Ok(skipped(Decision::Defer, Some(HINT.into())));
        };

        // Initial chat-turn payload. We re-build this every iteration
        // when the user picks `[c]hat` so the agent sees its prior
        // proposal plus the new instruction (rvpm Mode A pattern).
        let mut user_prompt = ctx.spec.prompt.clone().unwrap_or_default();
        let mut prior_body: Option<String> = None;

        for turn in 0..=MAX_CHAT_TURNS {
            let req = AiRequest {
                system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
                user_prompt: user_prompt.clone(),
                current: ctx.current_body.clone(),
                incoming: ctx.rendered_body.clone(),
                template_diff: None,
                dst: ctx.dst_abs.clone(),
            };

            let response = agent.run(req).await?;
            let body = match response.full_body {
                Some(b) => b,
                None => {
                    return Ok(failed(
                        "AI response did not include a <kata:body>...</kata:body> block; \
                         leaving destination untouched",
                    ));
                }
            };

            // No-op fast path: the agent produced what's already
            // on disk. Don't bother prompting — there's nothing to
            // accept.
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

            // Non-interactive `--yes`: accept the very first body.
            if !ctx.interactive {
                return write_and_return(ctx, &body, diff).await;
            }

            // Interactive: show the diff, then ask.
            eprintln!("\n=== AI proposal for {} ===", ctx.dst_abs);
            eprintln!("{diff}");

            match prompt_ai_decision(ctx.dst_abs.as_str())? {
                AiDecision::Accept => {
                    return write_and_return(ctx, &body, diff).await;
                }
                AiDecision::Skip => {
                    return Ok(skipped(Decision::Skip, None));
                }
                AiDecision::Defer => {
                    return Ok(skipped(Decision::Defer, None));
                }
                AiDecision::Chat(instr) => {
                    if turn == MAX_CHAT_TURNS {
                        eprintln!(
                            "  ai chat {}: {MAX_CHAT_TURNS} refinements reached; deferring",
                            ctx.dst_abs
                        );
                        return Ok(skipped(Decision::Defer, None));
                    }
                    // Carry the prior proposal + new instruction
                    // back into the next chat turn so the agent
                    // has full context.
                    let prev = prior_body.take().unwrap_or(body);
                    user_prompt = format!(
                        "{base}\n\n[prior AI proposal]\n{prev}\n\n[user refinement]\n{instr}",
                        base = ctx.spec.prompt.clone().unwrap_or_default(),
                    );
                    prior_body = Some(prev);
                    continue;
                }
            }
        }

        // Loop fell through without returning — should be
        // unreachable because every match arm above either returns
        // or `continue`s under `MAX_CHAT_TURNS`. Be defensive.
        Ok(skipped(Decision::Defer, None))
    }
}

fn skipped(decision: Decision, error: Option<String>) -> ActionOutcome {
    ActionOutcome {
        kind: OutcomeKind::Skipped,
        decision: Some(decision),
        diff: None,
        error,
    }
}

fn failed(msg: &str) -> ActionOutcome {
    ActionOutcome {
        kind: OutcomeKind::Failed,
        decision: None,
        diff: None,
        error: Some(msg.into()),
    }
}

async fn write_and_return(
    ctx: &ActionContext<'_>,
    body: &str,
    diff: String,
) -> Result<ActionOutcome> {
    if let Some(parent) = ctx.dst_abs.parent() {
        tokio::fs::create_dir_all(parent.as_std_path())
            .await
            .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
    }
    tokio::fs::write(ctx.dst_abs.as_std_path(), body)
        .await
        .map_err(|e| Error::io_at(ctx.dst_abs.as_std_path(), e))?;

    Ok(ActionOutcome {
        kind: OutcomeKind::Wrote,
        decision: Some(Decision::Accept),
        diff: Some(diff),
        error: None,
    })
}
