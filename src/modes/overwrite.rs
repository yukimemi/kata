//! `how = "overwrite"` — write the rendered body to the destination,
//! creating or replacing whatever's there.

use async_trait::async_trait;

use crate::error::{Error, Result};

use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

pub struct Overwrite;

#[async_trait]
impl ApplyMode for Overwrite {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan> {
        match &ctx.current_body {
            None => Ok(ActionPlan {
                kind: PlanKind::Create,
                diff: Some(unified_diff("", &ctx.rendered_body, ctx.dst_abs.as_str())),
            }),
            Some(cur) if *cur == ctx.rendered_body => Ok(ActionPlan {
                kind: PlanKind::Unchanged,
                diff: None,
            }),
            Some(cur) => Ok(ActionPlan {
                kind: PlanKind::Update,
                diff: Some(unified_diff(cur, &ctx.rendered_body, ctx.dst_abs.as_str())),
            }),
        }
    }

    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome> {
        let plan = self.plan(ctx).await?;
        match plan.kind {
            PlanKind::Unchanged => Ok(ActionOutcome {
                kind: OutcomeKind::Unchanged,
                decision: None,
                diff: None,
                error: None,
            }),
            PlanKind::Create | PlanKind::Update if dry_run => Ok(ActionOutcome {
                kind: OutcomeKind::Skipped,
                decision: None,
                diff: plan.diff,
                error: None,
            }),
            PlanKind::Create | PlanKind::Update => {
                if let Some(parent) = ctx.dst_abs.parent() {
                    tokio::fs::create_dir_all(parent.as_std_path())
                        .await
                        .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
                }
                tokio::fs::write(ctx.dst_abs.as_std_path(), &ctx.rendered_body)
                    .await
                    .map_err(|e| Error::io_at(ctx.dst_abs.as_std_path(), e))?;
                Ok(ActionOutcome {
                    kind: OutcomeKind::Wrote,
                    decision: None,
                    diff: plan.diff,
                    error: None,
                })
            }
            // Overwrite never produces these in `plan` (the runner
            // gates on `when` and the once-adoption fast path
            // before invoking us), but be explicit in case callers
            // compose plans externally.
            PlanKind::SkippedWhen | PlanKind::SkippedOnce | PlanKind::Diverged => {
                Ok(ActionOutcome {
                    kind: OutcomeKind::Skipped,
                    decision: None,
                    diff: plan.diff,
                    error: None,
                })
            }
            // Map adoption plans onto the matching outcome so the
            // ActionPlan -> ActionOutcome translation stays
            // consistent for any external composer that goes
            // through this mode.
            PlanKind::AdoptedExisting => Ok(ActionOutcome {
                kind: OutcomeKind::Adopted,
                decision: None,
                diff: plan.diff,
                error: None,
            }),
        }
    }
}
