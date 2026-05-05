//! `how = "ai"` — delegate the merge to an external agent CLI
//! (claude / gemini / codex / opencode) via the `AiAgent` trait.
//!
//! Decision matrix:
//!
//! - `dry_run`              → Skipped + Defer  (no agent round-trip)
//! - no agent on PATH       → Skipped + Defer + stderr hint
//! - non-interactive
//!   - `--yes`              → run agent, accept the body verbatim
//!   - no `--yes`           → Skipped + Defer (CI-safe default)
//! - interactive
//!   - run agent, show the diff, prompt with `interactive::prompt_ai_decision`:
//!     - `[a]ccept`         → write the body
//!     - `[e]dit`           → open the body in `$EDITOR` and write the
//!       edited result (no further AI calls)
//!     - `[c]hat <instr>`   → re-run with the instruction appended
//!       and the latest proposal carried forward in the prompt history
//!     - `[h]andoff`        → spawn the agent CLI interactively;
//!       kata stops re-importing
//!     - `[s]kip`           → Skipped + Skip
//!     - `[d]efer`          → Skipped + Defer
//!
//! `--ai-prompt <msg>` (run-wide instruction prepended to every
//! `how = "ai"` request) is honoured by stacking it before the
//! manifest-author user prompt.

use async_trait::async_trait;

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::ai::{AiRequest, DEFAULT_SYSTEM_PROMPT, run_handoff};
use crate::applied::Decision;
use crate::error::{Error, Result};
use crate::interactive::{AiDecision, prompt_ai_decision};
use crate::manifest::AiMode;

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

        // Resolve the per-file AI mode. CLI override wins; otherwise
        // honour what the manifest declared (default `Chat`).
        let resolved_mode = ctx
            .ai_mode_override
            .or(ctx.spec.ai_mode)
            .unwrap_or_default();

        // Manifest-elected handoff: skip the chat loop entirely and
        // hand the user straight to the agent CLI. We assemble the
        // prompt the same way the chat loop would have for turn 1
        // (run-wide --ai-prompt + per-file prompt) so the agent has
        // the same starting context — only kata's chat orchestration
        // is dropped, not the framing. The chat-side `agent` clone
        // above goes unused on this branch; that's fine, the
        // compiler/clippy treat its other branch usage as enough.
        if matches!(resolved_mode, AiMode::Handoff) {
            let Some(backend) = ctx.agent_backend else {
                return Ok(skipped(
                    Decision::Defer,
                    Some("handoff requested but no resolved AI backend was available".into()),
                ));
            };
            let user_prompt = compose_user_prompt(ctx.ai_prompt, ctx.spec.prompt.as_deref());
            let handoff_prompt = build_handoff_prompt_initial(&user_prompt, ctx);
            // Gate the handoff spawn against the global AI
            // concurrency cap (default 4). The permit is dropped
            // when the agent process exits.
            let _permit = acquire_ai_permit(&ctx.ai_sema).await?;
            run_handoff(backend, &handoff_prompt, ctx.dst_abs.as_std_path()).await?;
            return Ok(skipped(Decision::Defer, None));
        }

        // Initial chat-turn payload. We re-build this every iteration
        // when the user picks `[c]hat` so the agent sees its **most
        // recent** proposal plus the new instruction (rvpm Mode A
        // pattern). `prior_body` was deliberately removed: stashing
        // the proposal across turns made Turn 2+ feed the
        // *first-turn* body back instead of whatever the previous
        // turn just produced (Gemini high-priority).
        //
        // `--ai-prompt <msg>` stacks before the manifest's `prompt`
        // for every chat turn so the run-wide guidance survives
        // through `[c]hat` refinements without being lost when the
        // refinement payload is rebuilt.
        let base_prompt = compose_user_prompt(ctx.ai_prompt, ctx.spec.prompt.as_deref());
        let mut user_prompt = base_prompt.clone();

        for turn in 0..=MAX_CHAT_TURNS {
            let req = AiRequest {
                system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
                user_prompt: user_prompt.clone(),
                current: ctx.current_body.clone(),
                incoming: ctx.rendered_body.clone(),
                template_diff: None,
                dst: ctx.dst_abs.clone(),
            };

            // Gate every chat turn against the AI concurrency cap.
            // The permit is held only for this turn; chat refinement
            // turns each acquire fresh so a stuck agent can't keep
            // the gate locked between calls.
            let response = {
                let _permit = acquire_ai_permit(&ctx.ai_sema).await?;
                agent.run(req).await?
            };
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
                AiDecision::Edit => {
                    let edited = edit_in_editor(ctx.dst_abs.as_str(), &body)?;
                    if edited == body {
                        // No-op edit: just write the original.
                        return write_and_return(ctx, &body, diff).await;
                    }
                    let new_diff = unified_diff(
                        ctx.current_body.as_deref().unwrap_or(""),
                        &edited,
                        ctx.dst_abs.as_str(),
                    );
                    return write_and_return(ctx, &edited, new_diff).await;
                }
                AiDecision::Handoff => {
                    let Some(backend) = ctx.agent_backend else {
                        return Ok(skipped(
                            Decision::Defer,
                            Some(
                                "handoff requested but no resolved AI backend was available".into(),
                            ),
                        ));
                    };
                    // Hand the agent the same prompt kata would
                    // have driven through `invoke_chat`, plus a
                    // pointer to the destination file. After this
                    // returns kata does NOT re-import; the agent's
                    // own Edit / Write tools are responsible for
                    // updating the dst.
                    let handoff_prompt = build_handoff_prompt(&user_prompt, ctx, &body);
                    let _permit = acquire_ai_permit(&ctx.ai_sema).await?;
                    run_handoff(backend, &handoff_prompt, ctx.dst_abs.as_std_path()).await?;
                    return Ok(skipped(Decision::Defer, None));
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
                    // Carry the *latest* proposal + the new
                    // instruction into the next chat turn. We
                    // always reset to `base_prompt` first — never
                    // accumulate previous refinements — so an
                    // 8-turn session doesn't blow up the prompt
                    // size or confuse the agent with stale
                    // guidance.
                    user_prompt = format!(
                        "{base_prompt}\n\n[prior AI proposal]\n{body}\n\n[user refinement]\n{instr}"
                    );
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

/// Combine the run-wide `--ai-prompt` (when present) with the
/// per-file manifest `prompt` so the agent sees both in one
/// coherent instruction block. Either side may be empty or
/// whitespace-only; both are trimmed so the output never starts /
/// ends with stray newlines and an all-blank input collapses to
/// `String::new()`.
fn compose_user_prompt(run_wide: Option<&str>, per_file: Option<&str>) -> String {
    let r = run_wide.map(str::trim).filter(|s| !s.is_empty());
    let p = per_file.map(str::trim).filter(|s| !s.is_empty());
    match (r, p) {
        (Some(r), Some(p)) => {
            format!("[run-wide instruction]\n{r}\n\n[per-file instruction]\n{p}")
        }
        (Some(r), None) => r.to_string(),
        (None, Some(p)) => p.to_string(),
        (None, None) => String::new(),
    }
}

/// Open the AI-proposed `body` in the user's `$EDITOR` (or
/// `$VISUAL`) for in-place editing, then return the saved result.
/// The temp file extension matches the destination's so editors
/// pick the right syntax mode (e.g. `.md` for CLAUDE.md merges).
fn edit_in_editor(dst_label: &str, body: &str) -> Result<String> {
    use std::io::Write as _;

    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });

    let suffix = std::path::Path::new(dst_label)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_else(|| ".tmp".to_string());

    let mut tmp = tempfile::Builder::new()
        .prefix("kata-ai-edit-")
        .suffix(&suffix)
        .tempfile()
        .map_err(|e| {
            Error::Other(anyhow::Error::from(e).context("creating tmp file for $EDITOR"))
        })?;
    tmp.write_all(body.as_bytes()).map_err(|e| {
        Error::Other(anyhow::Error::from(e).context("seeding tmp file with AI body"))
    })?;
    let path = tmp.into_temp_path();

    // Spawn the editor with stdio inherited from the parent TTY so
    // the user can interact with it normally. `$EDITOR` commonly
    // carries arguments ("code --wait", "nvim --noplugin"), so we
    // split on whitespace and treat the first token as the
    // executable. POSIX shells parse with quoting rules; we don't
    // — paths with embedded spaces in `$EDITOR` need to use
    // `$VISUAL` set to a quoted-free wrapper or a non-spaced shim.
    let mut parts = editor.split_whitespace();
    let prog = parts.next().ok_or_else(|| {
        Error::Other(anyhow::anyhow!(
            "editor command resolved to an empty string after trimming"
        ))
    })?;
    let extra_args: Vec<&str> = parts.collect();
    let status = std::process::Command::new(prog)
        .args(&extra_args)
        .arg(path.as_os_str())
        .status()
        .map_err(|e| {
            Error::Other(
                anyhow::Error::from(e).context(format!("failed to spawn editor `{editor}`")),
            )
        })?;
    if !status.success() {
        return Err(Error::Other(anyhow::anyhow!(
            "editor `{editor}` exited with status {status} — leaving destination untouched",
        )));
    }

    let edited = std::fs::read_to_string(&path)
        .map_err(|e| Error::Other(anyhow::Error::from(e).context("reading edited tmp file")))?;
    // tmp file dropped here; tempfile cleans up.
    Ok(edited)
}

/// Acquire a permit on the run-wide AI semaphore, mapping
/// `tokio::sync::AcquireError` (the only failure mode — a closed
/// sema, which we never close) into a kata `Error`. Centralised so
/// the three call sites that gate AI work
/// (manifest-elected handoff, chat turn, in-dialog handoff) keep
/// reading top-to-bottom.
async fn acquire_ai_permit(sema: &Semaphore) -> Result<SemaphorePermit<'_>> {
    sema.acquire().await.map_err(|e| {
        Error::Other(anyhow::Error::from(e).context("acquiring AI concurrency permit"))
    })
}

/// Initial-handoff variant of `build_handoff_prompt`. Used when
/// the file's `ai_mode = "handoff"` (or `--ai-mode handoff`) is
/// in effect, so kata never ran a chat turn and there's no AI
/// proposal yet — only the rendered template body to hand over.
fn build_handoff_prompt_initial(user_prompt: &str, ctx: &ActionContext<'_>) -> String {
    let current = ctx.current_body.as_deref().unwrap_or("");
    format!(
        "{user_prompt}\n\n\
         [destination]\n{dst}\n\n\
         [current contents on disk]\n{current}\n\n\
         [freshly-rendered template body]\n{incoming}\n\n\
         The user has chosen handoff: kata will not re-import your output. \
         Use your own Edit / Write tools to merge the rendered template into \
         the destination directly.",
        dst = ctx.dst_abs,
        incoming = ctx.rendered_body,
    )
}

/// Build the prompt kata writes to the handoff tmp file. Stacks
/// the assembled user prompt with the latest AI proposal so the
/// agent has continuity even though kata's chat session ends here.
fn build_handoff_prompt(user_prompt: &str, ctx: &ActionContext<'_>, body: &str) -> String {
    let current = ctx.current_body.as_deref().unwrap_or("");
    format!(
        "{user_prompt}\n\n\
         [destination]\n{dst}\n\n\
         [current contents on disk]\n{current}\n\n\
         [latest AI proposal — kata is handing this conversation off to you]\n{body}\n\n\
         The user has chosen handoff: kata will not re-import your output. \
         Use your own Edit / Write tools to update the destination directly.",
        dst = ctx.dst_abs,
    )
}

#[cfg(test)]
mod tests {
    use super::compose_user_prompt;

    #[test]
    fn compose_user_prompt_returns_empty_when_neither_provided() {
        assert_eq!(compose_user_prompt(None, None), "");
        assert_eq!(compose_user_prompt(Some(""), Some("")), "");
    }

    #[test]
    fn compose_user_prompt_returns_only_run_wide_when_per_file_missing() {
        assert_eq!(
            compose_user_prompt(Some("respond in Japanese"), None),
            "respond in Japanese"
        );
        assert_eq!(
            compose_user_prompt(Some("respond in Japanese"), Some("")),
            "respond in Japanese"
        );
    }

    #[test]
    fn compose_user_prompt_returns_only_per_file_when_run_wide_missing() {
        assert_eq!(
            compose_user_prompt(None, Some("merge CLAUDE.md")),
            "merge CLAUDE.md"
        );
    }

    #[test]
    fn compose_user_prompt_combines_both_in_labelled_blocks() {
        let out = compose_user_prompt(Some("be terse"), Some("merge CLAUDE.md"));
        assert!(
            out.contains("[run-wide instruction]\nbe terse"),
            "missing run-wide block: {out}"
        );
        assert!(
            out.contains("[per-file instruction]\nmerge CLAUDE.md"),
            "missing per-file block: {out}"
        );
    }

    #[test]
    fn compose_user_prompt_trims_whitespace_only_inputs_to_empty() {
        // The docstring promises both sides are trimmed; a
        // whitespace-only string must collapse to `None`-like
        // behaviour so we don't emit dangling labels.
        assert_eq!(compose_user_prompt(Some("   \n  "), None), "");
        assert_eq!(compose_user_prompt(None, Some("\t\n")), "");
        assert_eq!(
            compose_user_prompt(Some("  hi  "), Some("\nbye\n")),
            "[run-wide instruction]\nhi\n\n[per-file instruction]\nbye"
        );
    }
}
