//! `kata update [<template>...] [--rev <ref>] [--at <dir>]`
//!  `kata update --all [--tag <t>] [--rev <ref>]`
//!
//! Refresh the cache slot for one or all git-sourced templates:
//! `git fetch --prune` to grab new commits, then `git checkout`
//! the requested rev (or the cache's current default). Updates
//! `applied.toml.templates[].rev` to the new resolved SHA.
//!
//! Local templates (`./...`, absolute paths) are skipped — they
//! are the truth of their own content; nothing to fetch.
//!
//! With `--all`, walks `GlobalConfig.projects` and fans out
//! across registered PJs in parallel (capped by
//! `defaults.pj_concurrency`).

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::applied::AppliedState;
use crate::config::GlobalConfig;
use crate::error::{Error, Result};
use crate::git;
use crate::template::{TemplateCache, source::normalise_git_url};

use super::{resolve_pj_concurrency, resolve_pj_root, select_registered_projects};

pub async fn run(
    templates_filter: Vec<String>,
    rev_override: Option<String>,
    at: Option<Utf8PathBuf>,
    no_color: bool,
) -> Result<()> {
    let _ = no_color;
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;

    let report = update_one_at(&pj_root, &templates_filter, &rev_override).await?;
    for line in report {
        println!("{line}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_all(
    tag_filter: Vec<String>,
    rev_override: Option<String>,
    pj_concurrency_override: Option<usize>,
    no_color: bool,
) -> Result<()> {
    let _ = no_color;
    let config = GlobalConfig::load()?;
    let projects = select_registered_projects(&config, &tag_filter);
    if projects.is_empty() {
        if tag_filter.is_empty() {
            println!(
                "no projects registered yet — `kata register` from inside a kata-managed PJ to add one."
            );
        } else {
            println!("no registered projects matched all of: {tag_filter:?}");
        }
        return Ok(());
    }

    let pj_concurrency = resolve_pj_concurrency(pj_concurrency_override);
    let sema = Arc::new(Semaphore::new(pj_concurrency.max(1)));

    let mut set = JoinSet::new();
    for entry in projects {
        let sema = sema.clone();
        let rev_override = rev_override.clone();
        set.spawn(async move {
            let _permit = sema.acquire_owned().await.expect("sema closed");
            let label = entry.name.clone();
            let path = entry.path.clone();
            // `--all` doesn't take a per-template filter — that
            // axis is per-call, not per-PJ. Always update every
            // template each PJ has applied.
            let outcome = update_one_at(&path, &[], &rev_override).await;
            (label, path, outcome)
        });
    }

    let mut total_errors = 0usize;
    while let Some(joined) = set.join_next().await {
        let (label, path, result) = match joined {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[panic] join task: {e}");
                total_errors += 1;
                continue;
            }
        };
        println!("\n=== {label} ({path}) ===");
        match result {
            Ok(report) => {
                for line in &report {
                    println!("{line}");
                    if line.starts_with("FAIL ") {
                        total_errors += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("  error: {e}");
                total_errors += 1;
            }
        }
    }

    if total_errors > 0 {
        return Err(Error::Other(anyhow::anyhow!(
            "{total_errors} template(s) failed across the registry"
        )));
    }
    Ok(())
}

/// Run the per-template fetch / checkout / rev-bump loop against
/// a single project root, returning a report (one line per
/// template). Pure data: no stdout writes here so the caller can
/// route the output (immediate `println!` for single-PJ, grouped
/// per-section for `--all`).
async fn update_one_at(
    pj_root: &Utf8Path,
    templates_filter: &[String],
    rev_override: &Option<String>,
) -> Result<Vec<String>> {
    let mut applied = AppliedState::load(pj_root)?;
    if applied.templates.is_empty() {
        return Err(Error::Config(format!(
            "{pj_root}: applied.toml has no templates recorded"
        )));
    }

    let cache = TemplateCache::ensure()?;
    let filter_active = !templates_filter.is_empty();
    let mut report: Vec<String> = Vec::new();

    for tmpl in applied.templates.iter_mut() {
        if filter_active
            && !templates_filter
                .iter()
                .any(|f| crate::cmd::remove::template_matches_pub(&tmpl.source, f))
        {
            continue;
        }
        if is_local_source(&tmpl.source) {
            report.push(format!("skip local: {}", tmpl.source));
            continue;
        }

        let url = normalise_git_url(&tmpl.source);
        let slot = cache.slot(&url);

        // Per-template failures push a FAIL line and continue so a
        // single broken upstream doesn't abort the whole run.
        if !slot.join(".git").is_dir() {
            if let Some(parent) = slot.parent() {
                if let Err(e) = std::fs::create_dir_all(parent.as_std_path()) {
                    report.push(format!("FAIL {}: mkdir {parent}: {e}", tmpl.source));
                    continue;
                }
            }
            if slot.exists() {
                let is_dir = std::fs::symlink_metadata(slot.as_std_path())
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                let rm = if is_dir {
                    std::fs::remove_dir_all(slot.as_std_path())
                } else {
                    std::fs::remove_file(slot.as_std_path())
                };
                if let Err(e) = rm {
                    report.push(format!("FAIL {}: clean {slot}: {e}", tmpl.source));
                    continue;
                }
            }
            if let Err(e) = git::clone_at(&url, slot.as_path()).await {
                report.push(format!("FAIL {}: clone: {e}", tmpl.source));
                continue;
            }
        } else if let Err(e) = git::fetch(slot.as_path()).await {
            report.push(format!("FAIL {}: fetch: {e}", tmpl.source));
            continue;
        }

        // No `--rev` → follow the upstream's default branch.
        // `HEAD` here would resolve to the **local** HEAD of the
        // cache slot (kata clones leave the slot in detached-HEAD
        // state, frozen at the last-applied SHA), so a plain
        // `git fetch && git checkout HEAD` is a no-op even when
        // origin has moved on. `origin/HEAD` is the symref
        // `git clone` sets up pointing at the remote's default
        // branch tip; checking it out post-fetch is what actually
        // advances the cache.
        let target = match rev_override {
            Some(r) => r.clone(),
            None => "origin/HEAD".to_string(),
        };
        if let Err(e) = git::checkout(slot.as_path(), &target).await {
            report.push(format!("FAIL {}: {e}", tmpl.source));
            continue;
        }

        let new_sha = git::current_head(slot.as_path()).await?;
        let old_sha = tmpl.rev.clone();
        if new_sha == old_sha {
            report.push(format!(
                "up-to-date: {} @ {}",
                tmpl.source,
                short_sha(&new_sha)
            ));
        } else {
            report.push(format!(
                "updated: {} {} -> {}",
                tmpl.source,
                short_sha(&old_sha),
                short_sha(&new_sha)
            ));
            tmpl.rev = new_sha;
        }
    }

    applied.save(pj_root)?;
    Ok(report)
}

fn is_local_source(s: &str) -> bool {
    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') {
        return true;
    }
    if s.starts_with(".\\") || s.starts_with("..\\") || s.starts_with('\\') {
        return true;
    }
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn short_sha(s: &str) -> String {
    if s.len() >= 7 {
        s[..7].to_string()
    } else {
        s.to_string()
    }
}
