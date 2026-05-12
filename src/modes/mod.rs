//! `ApplyMode` — one impl per `how` value in the manifest. With
//! Phase 3-b2 every `HowMode` variant has a concrete impl, so the
//! `for_how` dispatcher is exhaustive and no longer needs a
//! `Unimplemented` fallback shim.

pub mod ai;
pub mod merge_json;
pub mod merge_section;
pub mod merge_toml;
pub mod merge_yaml;
pub mod overwrite;
pub mod script;

use std::sync::Arc;

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use tokio::sync::Semaphore;

use crate::ai::{AiAgent, Backend};
use crate::applied::Decision;
use crate::config::ProjectEntry;
use crate::error::Result;
use crate::manifest::{AiMode, FileSpec, HowMode};
use crate::template::TemplateHandle;

pub use ai::Ai;
pub use merge_json::MergeJson;
pub use merge_section::MergeSection;
pub use merge_toml::MergeToml;
pub use merge_yaml::MergeYaml;
pub use overwrite::Overwrite;
pub use script::Script;

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
    /// Backend the agent (if any) is using. The `[h]andoff` arm in
    /// `modes/ai.rs` needs this to call `ai::process::run_handoff`,
    /// which spawns the CLI directly rather than going through the
    /// `AiAgent` trait. Always `Some` whenever `agent` is `Some`.
    pub agent_backend: Option<Backend>,
    pub interactive: bool,
    /// `--yes` accepts AI-generated bodies non-interactively. The
    /// chezmoi-style per-file dialog (Phase 3-b3) flips this on
    /// per-file once the user picks `[a]ccept`.
    pub yes_all: bool,
    /// `--ai-prompt <msg>`: extra free-form instruction the user
    /// asks kata to prepend to every `how = "ai"` request. Useful
    /// for "always keep my Section X" / "respond in Japanese" /
    /// session-wide guidance. None when not provided.
    pub ai_prompt: Option<&'a str>,
    /// `--ai-mode <chat|handoff>`: run-wide override for the
    /// per-file `ai_mode` from the manifest. `Some(Handoff)` makes
    /// every `how = "ai"` file go straight to handoff regardless of
    /// the manifest, useful for sessions where the user wants to
    /// drive the agent CLI directly. `None` means "use whatever the
    /// manifest declares (default `Chat`)".
    pub ai_mode_override: Option<AiMode>,
    /// Global gate that caps how many AI calls (chat turns +
    /// handoffs + `[e]dit` round-trips) can be in flight at once.
    /// Always supplied by the runner — single-PJ flows still use
    /// it because the chat loop itself can fan out per-file once
    /// Phase 3-d's tokio orchestration lands. The default capacity
    /// is `defaults.ai_concurrency` (`4`) so a casual `kata apply`
    /// never spawns more than 4 agents simultaneously even on a
    /// repository with dozens of `how = "ai"` files.
    pub ai_sema: Arc<Semaphore>,
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
    /// `when = "once"` and dst already exists on first apply —
    /// adopt the consumer's existing content instead of overwriting
    /// it. Recorded as `once_applied = true` so subsequent applies
    /// keep skipping; `content_hash` stays unset since `once` is
    /// the consumer's free zone (drift-tracking it would just emit
    /// noise on every consumer edit).
    AdoptedExisting,
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
    /// `when = "once"` and dst already exists on first apply —
    /// kept as-is, `once_applied = true` recorded so subsequent
    /// applies skip. See `PlanKind::AdoptedExisting`.
    Adopted,
    Failed,
}

#[async_trait]
pub trait ApplyMode: Send + Sync {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan>;
    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome>;
}

/// Resolve a `how` value to a concrete `ApplyMode`. Match is
/// exhaustive — every `HowMode` variant has a working impl as of
/// Phase 3-b2.
pub fn for_how(how: HowMode) -> Box<dyn ApplyMode> {
    match how {
        HowMode::Overwrite => Box::new(Overwrite),
        HowMode::MergeSection => Box::new(MergeSection),
        HowMode::MergeToml => Box::new(MergeToml),
        HowMode::MergeYaml => Box::new(MergeYaml),
        HowMode::MergeJson => Box::new(MergeJson),
        HowMode::Ai => Box::new(Ai),
        HowMode::Script => Box::new(Script),
    }
}

/// Build a unified diff of `before` vs `after` using `similar`.
/// Returned as a string with no ANSI colour (color is applied at
/// the UI layer). Shared by `overwrite` and `merge-section` so
/// both produce identical-shaped diff output.
pub(crate) fn unified_diff(before: &str, after: &str, label: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(before, after);
    let mut out = String::new();
    out.push_str(&format!("--- {label} (current)\n"));
    out.push_str(&format!("+++ {label} (incoming)\n"));
    for hunk in diff.unified_diff().iter_hunks() {
        out.push_str(&format!("{hunk}"));
    }
    out
}
