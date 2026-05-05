//! `kata list` — by default, show what governs the current PJ;
//! with `--all`, walk the global registry and emit a one-row-per-PJ
//! overview instead.

use camino::Utf8PathBuf;
use owo_colors::OwoColorize;

use crate::applied::AppliedState;
use crate::config::GlobalConfig;
use crate::error::{Error, Result};
use crate::ui;

use super::resolve_pj_root;

pub fn run(at: Option<Utf8PathBuf>, all: bool, paths: bool, no_color: bool) -> Result<()> {
    if all {
        return run_all(paths, no_color);
    }
    run_single(at, no_color)
}

fn run_single(at: Option<Utf8PathBuf>, no_color: bool) -> Result<()> {
    let explicit_at = at.is_some();
    let cwd = resolve_pj_root(at)?;
    let pj_root = match crate::paths::find_pj_root(&cwd) {
        Some(p) => p,
        None if explicit_at => {
            return Err(Error::Config(format!(
                "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
            )));
        }
        None => return run_single_pick_from_registry(no_color),
    };
    let applied = AppliedState::load(&pj_root)?;

    println!("project: {pj_root}");
    if let Some(p) = &applied.preset {
        println!("preset:  {p}");
    }
    println!();
    println!("templates ({} applied):", applied.templates.len());
    for t in &applied.templates {
        let v = t.version.as_deref().unwrap_or("-");
        println!("  - {} @ {} (manifest version: {})", t.source, t.rev, v);
    }
    println!();
    println!("vars:");
    for (k, v) in &applied.vars {
        println!("  {k} = {v}");
    }
    println!();
    println!("files ({} tracked):", applied.files.len());
    for (k, fs) in &applied.files {
        let once = if fs.once_applied { " (once)" } else { "" };
        println!("  - {k}{once}");
    }
    Ok(())
}

fn run_all(show_paths: bool, no_color: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    if config.projects.is_empty() {
        println!(
            "no projects registered yet — `kata register` from inside a kata-managed PJ to add one."
        );
        return Ok(());
    }

    let rows: Vec<RegistryRow> = config
        .projects
        .iter()
        .map(RegistryRow::from_entry)
        .collect();
    let color = ui::color_enabled(no_color);

    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let path_w = rows.iter().map(|r| r.path.len()).max().unwrap_or(4).max(4);
    let preset_w = rows
        .iter()
        .map(|r| r.preset.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let templates_w = 9; // header label "TEMPLATES"
    let applied_w = rows
        .iter()
        .map(|r| r.applied_at.len())
        .max()
        .unwrap_or(7)
        .max(7);

    print_header(
        &[
            ("NAME", name_w),
            ("PATH", path_w),
            ("PRESET", preset_w),
            ("TEMPLATES", templates_w),
            ("APPLIED", applied_w),
            ("STATUS", 0),
        ],
        show_paths,
        color,
    );
    for r in &rows {
        let mut cells = vec![format!("{:<name_w$}", r.name, name_w = name_w)];
        if show_paths {
            cells.push(if color {
                format!("{:<path_w$}", r.path, path_w = path_w)
                    .dimmed()
                    .to_string()
            } else {
                format!("{:<path_w$}", r.path, path_w = path_w)
            });
        }
        cells.push(if color {
            format!("{:<preset_w$}", r.preset, preset_w = preset_w)
                .dimmed()
                .to_string()
        } else {
            format!("{:<preset_w$}", r.preset, preset_w = preset_w)
        });
        cells.push(format!(
            "{:<templates_w$}",
            r.templates,
            templates_w = templates_w
        ));
        cells.push(if color {
            format!("{:<applied_w$}", r.applied_at, applied_w = applied_w)
                .dimmed()
                .to_string()
        } else {
            format!("{:<applied_w$}", r.applied_at, applied_w = applied_w)
        });
        cells.push(format_status(&r.status, color));
        println!("{}", cells.join("  "));
    }
    Ok(())
}

fn print_header(cells: &[(&str, usize)], show_paths: bool, color: bool) {
    let mut parts = Vec::with_capacity(cells.len());
    for (label, width) in cells {
        if !show_paths && *label == "PATH" {
            continue;
        }
        let cell = if *width == 0 {
            (*label).to_string()
        } else {
            format!("{:<w$}", label, w = *width)
        };
        parts.push(if color {
            cell.bold().to_string()
        } else {
            cell
        });
    }
    println!("{}", parts.join("  "));
}

fn format_status(s: &str, color: bool) -> String {
    if !color {
        return s.to_string();
    }
    match s {
        "ok" => s.green().to_string(),
        "not init'd" => s.cyan().to_string(),
        s if s.starts_with("error") || s == "missing dir" => s.red().bold().to_string(),
        _ => s.to_string(),
    }
}

/// Fallback when `kata list` (no `--at`, no `--all`) is run from
/// a directory with no `.kata/applied.toml` in its hierarchy: pull
/// the global registry and offer an `inquire` select instead of
/// erroring. Matches `renri list`'s pattern.
fn run_single_pick_from_registry(no_color: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    if config.projects.is_empty() {
        return Err(Error::Config(
            "no .kata/applied.toml in the current directory's hierarchy and no projects in the global registry — \
             cd into a kata-managed PJ, or run `kata init` first."
                .into(),
        ));
    }
    // Disambiguate by path so two PJs with the same `name` don't
    // collapse into one menu entry.
    let labels: Vec<String> = config
        .projects
        .iter()
        .map(|p| format!("{}  ({})", p.name, p.path))
        .collect();
    let chosen = inquire::Select::new("pick a project to inspect:", labels.clone())
        .with_help_message("\u{2191}\u{2193} to move, Enter to confirm, Esc to cancel")
        .prompt()
        .map_err(|e| match e {
            inquire::InquireError::OperationCanceled
            | inquire::InquireError::OperationInterrupted => Error::Config("cancelled".into()),
            other => Error::Config(format!("prompt failed: {other}")),
        })?;
    let idx = labels
        .iter()
        .position(|l| l == &chosen)
        .expect("chosen label must come from labels");
    let selected = config.projects[idx].path.clone();
    run_single(Some(selected), no_color)
}

struct RegistryRow {
    name: String,
    path: String,
    preset: String,
    templates: String,
    applied_at: String,
    status: String,
}

impl RegistryRow {
    fn from_entry(entry: &crate::config::ProjectEntry) -> Self {
        let path = entry.path.as_str().to_string();
        // A registered PJ whose directory has been moved is a real-
        // world condition; surface it in STATUS rather than abort
        // the whole listing.
        if !entry.path.exists() {
            return Self {
                name: entry.name.clone(),
                path,
                preset: "-".into(),
                templates: "-".into(),
                applied_at: "-".into(),
                status: "missing dir".into(),
            };
        }
        match AppliedState::load(&entry.path) {
            Ok(applied) if applied.templates.is_empty() => Self {
                name: entry.name.clone(),
                path,
                preset: "(none)".into(),
                templates: "0".into(),
                applied_at: "never".into(),
                status: "not init'd".into(),
            },
            Ok(applied) => {
                let preset = applied
                    .preset
                    .clone()
                    .unwrap_or_else(|| "(none)".to_string());
                let applied_at = applied
                    .applied_at
                    .map(|t| format!("{t}"))
                    .unwrap_or_else(|| "never".to_string());
                Self {
                    name: entry.name.clone(),
                    path,
                    preset,
                    templates: applied.templates.len().to_string(),
                    applied_at,
                    status: "ok".into(),
                }
            }
            Err(e) => Self {
                name: entry.name.clone(),
                path,
                preset: "-".into(),
                templates: "-".into(),
                applied_at: "-".into(),
                status: format!("error: {e}"),
            },
        }
    }
}
