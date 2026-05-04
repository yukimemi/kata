//! `how = "merge-section"` — replace just the marker-bracketed
//! block inside an existing file. Useful for managing a small
//! kata-owned section of a larger file the project also edits by
//! hand: a `dependencies` block in `Cargo.toml`, a managed
//! section in `.gitignore`, a tagged region in `CLAUDE.md`.
//!
//! Behaviour:
//!
//! - **No file at dst** — write `<begin>\n<body>\n<end>\n`.
//! - **File at dst, both markers present** — replace bytes from
//!   the start of `<begin>` through the end of `<end>` with
//!   `<begin>\n<body>\n<end>`.
//! - **File at dst, only one marker present** — `Diverged` (a
//!   project edit broke the pair; the user should fix manually).
//! - **File at dst, neither marker present** — append
//!   `<begin>\n<body>\n<end>\n` to the file (with a leading
//!   newline if the existing tail isn't already terminated).

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::manifest::MarkerSpec;

use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

pub struct MergeSection;

#[async_trait]
impl ApplyMode for MergeSection {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan> {
        let marker = require_marker(ctx)?;
        match compose(marker, ctx.current_body.as_deref(), &ctx.rendered_body) {
            ComposeResult::Create(new_body) => Ok(ActionPlan {
                kind: PlanKind::Create,
                diff: Some(unified_diff("", &new_body, ctx.dst_abs.as_str())),
            }),
            ComposeResult::Unchanged => Ok(ActionPlan {
                kind: PlanKind::Unchanged,
                diff: None,
            }),
            ComposeResult::Update { current, new_body } => Ok(ActionPlan {
                kind: PlanKind::Update,
                diff: Some(unified_diff(&current, &new_body, ctx.dst_abs.as_str())),
            }),
            ComposeResult::Diverged => Ok(ActionPlan {
                kind: PlanKind::Diverged,
                diff: Some(format!(
                    "(only one of `{}` / `{}` present in {} — fix manually)",
                    marker.begin, marker.end, ctx.dst_abs
                )),
            }),
        }
    }

    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome> {
        let marker = require_marker(ctx)?;
        let composed = compose(marker, ctx.current_body.as_deref(), &ctx.rendered_body);

        match composed {
            ComposeResult::Unchanged => Ok(ActionOutcome {
                kind: OutcomeKind::Unchanged,
                decision: None,
                diff: None,
                error: None,
            }),
            ComposeResult::Diverged => Ok(ActionOutcome {
                kind: OutcomeKind::Failed,
                decision: None,
                diff: None,
                error: Some(format!(
                    "merge-section: only one of `{}` / `{}` present in {}; fix manually",
                    marker.begin, marker.end, ctx.dst_abs
                )),
            }),
            ComposeResult::Create(new_body) | ComposeResult::Update { new_body, .. } if dry_run => {
                Ok(ActionOutcome {
                    kind: OutcomeKind::Skipped,
                    decision: None,
                    diff: Some(unified_diff(
                        ctx.current_body.as_deref().unwrap_or(""),
                        &new_body,
                        ctx.dst_abs.as_str(),
                    )),
                    error: None,
                })
            }
            ComposeResult::Create(new_body) | ComposeResult::Update { new_body, .. } => {
                let diff = unified_diff(
                    ctx.current_body.as_deref().unwrap_or(""),
                    &new_body,
                    ctx.dst_abs.as_str(),
                );
                if let Some(parent) = ctx.dst_abs.parent() {
                    tokio::fs::create_dir_all(parent.as_std_path())
                        .await
                        .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
                }
                tokio::fs::write(ctx.dst_abs.as_std_path(), &new_body)
                    .await
                    .map_err(|e| Error::io_at(ctx.dst_abs.as_std_path(), e))?;
                // Same shape as `Overwrite::execute` — Wrote carries
                // the diff so the UI layer can replay it.
                Ok(ActionOutcome {
                    kind: OutcomeKind::Wrote,
                    decision: None,
                    diff: Some(diff),
                    error: None,
                })
            }
        }
    }
}

#[derive(Debug)]
enum ComposeResult {
    /// dst doesn't exist; write a fresh marker block.
    Create(String),
    /// dst exists, computed body differs from current.
    Update { current: String, new_body: String },
    /// dst exists, computed body equals current — no-op.
    Unchanged,
    /// One marker present, the other isn't — refuse to guess.
    Diverged,
}

fn compose(marker: &MarkerSpec, current: Option<&str>, body: &str) -> ComposeResult {
    let block = marker_block(marker, body);
    let current = match current {
        None => return ComposeResult::Create(format!("{block}\n")),
        Some(c) => c,
    };

    let begin = current.find(&marker.begin);
    let end = current.find(&marker.end);

    match (begin, end) {
        // `es >= bs + begin.len()` — the end marker must start
        // strictly *after* the begin marker ends, so a marker
        // pair that overlaps (or where begin == end as a literal
        // substring matching at the same position) falls through
        // to the catch-all Diverged arm.
        (Some(bs), Some(es)) if es >= bs + marker.begin.len() => {
            // Replace from start of `begin` through end of `end`.
            let end_pos = es + marker.end.len();
            let mut new_body = current[..bs].to_string();
            new_body.push_str(&block);
            new_body.push_str(&current[end_pos..]);
            if new_body == current {
                ComposeResult::Unchanged
            } else {
                ComposeResult::Update {
                    current: current.to_string(),
                    new_body,
                }
            }
        }
        (Some(_), None) | (None, Some(_)) => ComposeResult::Diverged,
        (Some(_), Some(_)) => {
            // Markers overlap, are reversed, or use the same
            // literal in both positions. Refuse to guess what the
            // user meant; surface as Diverged.
            ComposeResult::Diverged
        }
        (None, None) => {
            // Both markers absent — append.
            let sep = if current.ends_with('\n') || current.is_empty() {
                ""
            } else {
                "\n"
            };
            let new_body = format!("{current}{sep}{block}\n");
            if new_body == current {
                ComposeResult::Unchanged
            } else {
                ComposeResult::Update {
                    current: current.to_string(),
                    new_body,
                }
            }
        }
    }
}

/// `<begin>\n<body trimmed>\n<end>` — no trailing newline (the
/// caller adds one when this is the whole file body).
fn marker_block(marker: &MarkerSpec, body: &str) -> String {
    format!(
        "{}\n{}\n{}",
        marker.begin,
        body.trim_end_matches('\n'),
        marker.end
    )
}

fn require_marker<'a>(ctx: &'a ActionContext<'_>) -> Result<&'a MarkerSpec> {
    ctx.spec.marker.as_ref().ok_or_else(|| {
        Error::manifest(
            PathBuf::from(&ctx.template.source_spec),
            format!(
                "how=\"merge-section\" requires a `marker` table in `[[file]]` for {}",
                ctx.spec.src
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker() -> MarkerSpec {
        MarkerSpec {
            begin: "# >>> kata managed <<<".to_string(),
            end: "# <<< kata managed >>>".to_string(),
        }
    }

    #[test]
    fn create_when_dst_absent() {
        let r = compose(&marker(), None, "[deps]\nx = 1\n");
        match r {
            ComposeResult::Create(body) => {
                assert!(body.starts_with("# >>> kata managed <<<\n"));
                assert!(body.contains("[deps]\nx = 1"));
                assert!(body.contains("\n# <<< kata managed >>>\n"));
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn replace_existing_block_in_place() {
        let cur = "# header\n# >>> kata managed <<<\nold\n# <<< kata managed >>>\n# footer\n";
        let r = compose(&marker(), Some(cur), "new");
        match r {
            ComposeResult::Update { new_body, .. } => {
                assert!(new_body.contains("# header"));
                assert!(new_body.contains("# >>> kata managed <<<\nnew\n# <<< kata managed >>>"));
                assert!(new_body.contains("# footer"));
                assert!(!new_body.contains("old"));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_when_existing_block_matches() {
        let cur = "before\n# >>> kata managed <<<\nbody\n# <<< kata managed >>>\nafter\n";
        let r = compose(&marker(), Some(cur), "body\n");
        assert!(matches!(r, ComposeResult::Unchanged));
    }

    #[test]
    fn append_when_no_marker_in_existing() {
        let cur = "manual content\n";
        let r = compose(&marker(), Some(cur), "managed");
        match r {
            ComposeResult::Update { new_body, .. } => {
                assert!(new_body.starts_with("manual content\n"));
                assert!(
                    new_body.contains("# >>> kata managed <<<\nmanaged\n# <<< kata managed >>>\n")
                );
            }
            other => panic!("expected Update (append), got {other:?}"),
        }
    }

    #[test]
    fn append_inserts_separator_when_existing_lacks_trailing_newline() {
        let cur = "no trailing newline";
        let r = compose(&marker(), Some(cur), "managed");
        match r {
            ComposeResult::Update { new_body, .. } => {
                assert!(new_body.starts_with("no trailing newline\n# >>> kata managed <<<"));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn diverged_when_only_begin_marker_present() {
        let cur = "# >>> kata managed <<<\nbody but no end marker\n";
        let r = compose(&marker(), Some(cur), "body");
        assert!(matches!(r, ComposeResult::Diverged));
    }

    #[test]
    fn diverged_when_only_end_marker_present() {
        let cur = "no begin marker\n# <<< kata managed >>>\n";
        let r = compose(&marker(), Some(cur), "body");
        assert!(matches!(r, ComposeResult::Diverged));
    }

    #[test]
    fn diverged_when_markers_swapped() {
        let cur = "# <<< kata managed >>>\nbackwards\n# >>> kata managed <<<\n";
        let r = compose(&marker(), Some(cur), "body");
        assert!(matches!(r, ComposeResult::Diverged));
    }

    #[test]
    fn diverged_when_begin_and_end_are_identical_literals() {
        // Edge case the `es >= bs + begin.len()` check guards: if
        // `begin == end` as substrings, both `find` calls return
        // the same position and only ONE marker is actually present
        // — which under the old `es >= bs` check would have been
        // mistakenly treated as a valid pair.
        let m = MarkerSpec {
            begin: "// SAME //".to_string(),
            end: "// SAME //".to_string(),
        };
        let cur = "before\n// SAME //\nafter\n";
        let r = compose(&m, Some(cur), "body");
        assert!(matches!(r, ComposeResult::Diverged));
    }
}
