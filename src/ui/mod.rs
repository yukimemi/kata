//! Output formatting helpers. Phase 1 keeps this minimal — colour via
//! owo-colors, no icon mode dispatch yet (Phase 4 polish).

pub mod diff;

use std::io::IsTerminal;

use owo_colors::OwoColorize;

use crate::modes::{OutcomeKind, PlanKind};

/// True if stdout looks like a real TTY and `NO_COLOR` is not set.
pub fn color_enabled(no_color: bool) -> bool {
    if no_color || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// One-line status for a single applied file.
pub fn print_outcome(dst: &str, kind: OutcomeKind, no_color: bool) {
    let (label, plain) = match kind {
        OutcomeKind::Wrote => ("wrote", "wrote     "),
        OutcomeKind::Unchanged => ("unchanged", "unchanged "),
        OutcomeKind::Skipped => ("skipped", "skipped   "),
        OutcomeKind::Failed => ("failed", "failed    "),
    };
    let _ = label;
    if color_enabled(no_color) {
        match kind {
            OutcomeKind::Wrote => println!("  {} {}", "wrote    ".green().bold(), dst),
            OutcomeKind::Unchanged => println!("  {} {}", "unchanged".dimmed(), dst),
            OutcomeKind::Skipped => println!("  {} {}", "skipped  ".yellow(), dst),
            OutcomeKind::Failed => println!("  {} {}", "failed   ".red().bold(), dst),
        }
    } else {
        println!("  {plain}{dst}");
    }
}

/// One-line plan preview (used by `status` / `dry-run`).
pub fn print_plan(dst: &str, kind: PlanKind, no_color: bool) {
    let label = match kind {
        PlanKind::Create => "create",
        PlanKind::Update => "update",
        PlanKind::Unchanged => "ok",
        PlanKind::SkippedWhen => "skip(when)",
        PlanKind::SkippedOnce => "skip(once)",
        PlanKind::Diverged => "diverged",
    };
    if color_enabled(no_color) {
        let coloured = match kind {
            PlanKind::Create => format!("{:<10}", label).green().bold().to_string(),
            PlanKind::Update => format!("{:<10}", label).cyan().bold().to_string(),
            PlanKind::Unchanged => format!("{:<10}", label).dimmed().to_string(),
            PlanKind::SkippedWhen | PlanKind::SkippedOnce => {
                format!("{:<10}", label).yellow().to_string()
            }
            PlanKind::Diverged => format!("{:<10}", label).red().bold().to_string(),
        };
        println!("  {} {}", coloured, dst);
    } else {
        println!("  {:<10} {}", label, dst);
    }
}

/// Section header, e.g. for the project name above its file list.
pub fn print_pj_header(name: &str, path: &str, no_color: bool) {
    if color_enabled(no_color) {
        println!("\n{} {}", name.bold(), format!("({path})").dimmed());
    } else {
        println!("\n{name} ({path})");
    }
}
