//! `kata status [--at <dir>]` — preview what `kata apply` would do.
//! `kata status --all [--tag <t>]` — drift overview across the
//! global registry (every PJ kata knows about).

use camino::{Utf8Path, Utf8PathBuf};
use owo_colors::OwoColorize;

use crate::applied::AppliedState;
use crate::config::{GlobalConfig, ProjectEntry};
use crate::error::{Error, Result};
use crate::preset::TemplateRef;
use crate::runner::{hash_content, plan_pj};
use crate::ui;

use super::{resolve_pj_root, select_registered_projects};

pub async fn run(
    at: Option<Utf8PathBuf>,
    all: bool,
    tags: Vec<String>,
    paths: bool,
    interactive: bool,
    no_color: bool,
) -> Result<()> {
    if all {
        return run_all(tags, paths, no_color);
    }
    run_single(at, interactive, no_color).await
}

async fn run_single(at: Option<Utf8PathBuf>, interactive: bool, no_color: bool) -> Result<()> {
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;

    let applied = AppliedState::load(&pj_root)?;
    let templates: Vec<TemplateRef> = applied
        .templates
        .iter()
        .map(|t| TemplateRef {
            source: t.source.clone(),
            rev: Some(t.rev.clone()),
            subdir: t.subdir.clone(),
        })
        .collect();

    let project = ProjectEntry {
        name: pj_root.file_name().unwrap_or("kata-project").to_string(),
        path: pj_root.clone(),
        tags: vec![],
        overrides: None,
    };

    let base_dir = applied.base_dir.clone().unwrap_or(cwd);

    let plans = plan_pj(
        project,
        pj_root.clone(),
        templates,
        base_dir,
        toml::Table::new(),
        interactive,
        Default::default(),
    )
    .await?;

    ui::print_pj_header(
        pj_root.file_name().unwrap_or("project"),
        pj_root.as_str(),
        no_color,
    );
    for (dst, kind, _diff) in &plans {
        ui::print_plan(dst, *kind, no_color);
    }
    Ok(())
}

fn run_all(tags: Vec<String>, show_paths: bool, no_color: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    let projects = select_registered_projects(&config, &tags);
    if projects.is_empty() {
        if tags.is_empty() {
            println!(
                "no projects registered yet — `kata register` from inside a kata-managed PJ to add one."
            );
        } else {
            println!("no registered projects matched all of: {tags:?}");
        }
        return Ok(());
    }

    let rows: Vec<DriftRow> = projects.iter().map(DriftRow::from_entry).collect();
    let color = ui::color_enabled(no_color);

    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let path_w = rows.iter().map(|r| r.path.len()).max().unwrap_or(4).max(4);
    let tracked_w = 7;
    let drift_w = rows
        .iter()
        .map(|r| r.drift_summary.len())
        .max()
        .unwrap_or(5)
        .max(5);

    print_header(&[
        ("NAME", name_w),
        ("PATH", path_w),
        ("TRACKED", tracked_w),
        ("DRIFT", drift_w),
        ("STATUS", 0),
    ], show_paths, color);

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
        cells.push(format!(
            "{:<tracked_w$}",
            r.tracked,
            tracked_w = tracked_w
        ));
        cells.push(format_drift_summary(&r.drift_summary, drift_w, color));
        cells.push(format_status(&r.status, color));
        println!("{}", cells.join("  "));

        for line in &r.drift_detail {
            if color {
                println!("    {}", line.yellow());
            } else {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

/// Print the bold-but-uncoloured table header, optionally
/// dropping the PATH column.
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
        "drift" => s.yellow().bold().to_string(),
        "not init'd" => s.cyan().to_string(),
        s if s.starts_with("error") || s == "missing dir" => s.red().bold().to_string(),
        _ => s.to_string(),
    }
}

fn format_drift_summary(s: &str, width: usize, color: bool) -> String {
    let padded = format!("{:<w$}", s, w = width);
    if !color {
        return padded;
    }
    if s == "clean" {
        padded.green().to_string()
    } else if s.contains("drifted") {
        padded.yellow().to_string()
    } else {
        padded
    }
}

struct DriftRow {
    name: String,
    path: String,
    /// `<n>` — count of files with a recorded `content_hash` that
    /// kata can compare against on-disk bytes (drift-checkable).
    tracked: String,
    /// Either `clean` or `<n> drifted` for the column.
    drift_summary: String,
    /// `ok` / `drift` / `not init'd` / `missing dir` / `error: …`.
    status: String,
    /// Per-file lines printed under the row when there's drift.
    drift_detail: Vec<String>,
}

impl DriftRow {
    fn from_entry(entry: &ProjectEntry) -> Self {
        let path = entry.path.as_str().to_string();
        if !entry.path.exists() {
            return Self {
                name: entry.name.clone(),
                path,
                tracked: "-".into(),
                drift_summary: "-".into(),
                status: "missing dir".into(),
                drift_detail: vec![],
            };
        }
        let applied = match AppliedState::load(&entry.path) {
            Ok(a) => a,
            Err(e) => {
                return Self {
                    name: entry.name.clone(),
                    path,
                    tracked: "-".into(),
                    drift_summary: "-".into(),
                    status: format!("error: {e}"),
                    drift_detail: vec![],
                };
            }
        };
        if applied.templates.is_empty() {
            return Self {
                name: entry.name.clone(),
                path,
                tracked: "0".into(),
                drift_summary: "-".into(),
                status: "not init'd".into(),
                drift_detail: vec![],
            };
        }

        let (tracked, drift_detail) = check_drift(&entry.path, &applied);
        let drift_summary = if drift_detail.is_empty() {
            "clean".into()
        } else {
            format!("{} drifted", drift_detail.len())
        };
        let status = if drift_detail.is_empty() {
            "ok".into()
        } else {
            "drift".into()
        };
        Self {
            name: entry.name.clone(),
            path,
            tracked: tracked.to_string(),
            drift_summary,
            status,
            drift_detail,
        }
    }
}

/// For each file kata is tracking on this PJ, compare the
/// recorded `content_hash` to the SHA-256 of what's on disk.
/// Returns `(tracked_count, drift_lines)`.
fn check_drift(pj_root: &Utf8Path, applied: &AppliedState) -> (usize, Vec<String>) {
    let mut tracked = 0;
    let mut drift = Vec::new();
    for (dst_rel, file_state) in &applied.files {
        let Some(expected) = file_state.content_hash.as_deref() else {
            continue;
        };
        tracked += 1;
        let dst_abs = pj_root.join(dst_rel);
        match std::fs::read(dst_abs.as_std_path()) {
            Ok(body) => {
                let actual = hash_content(&body);
                if actual != expected {
                    drift.push(format!(
                        "{dst_rel}  (modified — disk diverges from applied.toml)"
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                drift.push(format!("{dst_rel}  (missing — file deleted since apply)"));
            }
            Err(e) => {
                drift.push(format!("{dst_rel}  (unreadable — {e})"));
            }
        }
    }
    (tracked, drift)
}
