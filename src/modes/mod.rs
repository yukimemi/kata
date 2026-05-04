//! `ApplyMode` — one impl per `how` value in the manifest.
//!
//! Phase 1 ships only `Overwrite`; the other variants resolve to an
//! `Unimplemented` shim that errors clearly so the runtime can keep
//! the trait-object dispatch shape stable.

pub mod overwrite;

use std::sync::Arc;

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};

use crate::ai::AiAgent;
use crate::applied::Decision;
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::manifest::{FileSpec, HowMode};
use crate::template::TemplateHandle;

pub use overwrite::Overwrite;

/// Inputs available to every `ApplyMode` invocation.
pub struct ActionContext<'a> {
    pub project: &'a ProjectEntry,
    pub pj_root: &'a Utf8Path,
    pub template: &'a TemplateHandle,
    pub spec: &'a FileSpec,
    /// Absolute path to the source file inside the template root.
    pub src_abs: Utf8PathBuf,
    /// Absolute path to the destination inside the project root.
    pub dst_abs: Utf8PathBuf,
    /// Newly-rendered template body (already passed through Tera).
    pub rendered_body: String,
    /// Current destination contents, if the file exists.
    pub current_body: Option<String>,
    pub vars: &'a toml::Table,
    pub tera_ctx: &'a tera::Context,
    /// Resolved AI agent (only meaningful for `how = "ai"`).
    pub agent: Option<Arc<dyn AiAgent>>,
    pub interactive: bool,
}

/// What a mode reports during `plan` (read-only preview).
#[derive(Debug, Clone)]
pub struct ActionPlan {
    pub kind: PlanKind,
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    /// dst doesn't exist; would create
    Create,
    /// dst exists with different content; would update
    Update,
    /// dst exists with identical content; no-op
    Unchanged,
    /// `when_expr` evaluated to false
    SkippedWhen,
    /// `when = "once"` already applied
    SkippedOnce,
    /// dst content has diverged in a way the mode can't auto-resolve
    Diverged,
}

#[derive(Debug, Clone)]
pub struct ActionOutcome {
    pub kind: OutcomeKind,
    pub decision: Option<Decision>,
    pub diff: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Wrote,
    Unchanged,
    Skipped,
    Failed,
}

#[async_trait]
pub trait ApplyMode: Send + Sync {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan>;
    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome>;
}

/// Resolve a `how` value to a concrete `ApplyMode`. Phase 1 returns
/// the working `Overwrite` impl for `Overwrite`, and an
/// always-erroring shim for everything else.
pub fn for_how(how: HowMode) -> Box<dyn ApplyMode> {
    match how {
        HowMode::Overwrite => Box::new(Overwrite),
        other => Box::new(Unimplemented(other)),
    }
}

struct Unimplemented(HowMode);

#[async_trait]
impl ApplyMode for Unimplemented {
    async fn plan(&self, _ctx: &ActionContext<'_>) -> Result<ActionPlan> {
        Err(unimpl_err(self.0))
    }
    async fn execute(&self, _ctx: &ActionContext<'_>, _dry_run: bool) -> Result<ActionOutcome> {
        Err(unimpl_err(self.0))
    }
}

fn unimpl_err(how: HowMode) -> Error {
    Error::Other(anyhow::anyhow!(
        "how = {:?} is not implemented yet (Phase 1 ships overwrite only)",
        how
    ))
}
