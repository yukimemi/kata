//! `kata update [<template>...] [--rev <ref>] [--at <dir>]`
//!
//! Refresh the cache slot for one or all git-sourced templates:
//! `git fetch --prune` to grab new commits, then `git checkout`
//! the requested rev (or the cache's current default). Updates
//! `applied.toml.templates[].rev` to the new resolved SHA.
//!
//! Local templates (`./...`, absolute paths) are skipped — they
//! are the truth of their own content; nothing to fetch.
//!
//! Phase 4 will add `--apply` to chain this into a re-apply run.

use camino::Utf8PathBuf;

use crate::applied::AppliedState;
use crate::error::{Error, Result};
use crate::git;
use crate::template::{TemplateCache, source::normalise_git_url};

use super::resolve_pj_root;

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

    let mut applied = AppliedState::load(&pj_root)?;
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

        // Make sure the slot is a real git repo; otherwise (re-)clone.
        if !slot.join(".git").is_dir() {
            if let Some(parent) = slot.parent() {
                std::fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
            }
            if slot.exists() {
                std::fs::remove_dir_all(slot.as_std_path())
                    .map_err(|e| Error::io_at(slot.as_std_path(), e))?;
            }
            git::clone_at(&url, slot.as_path()).await?;
        } else {
            git::fetch(slot.as_path()).await?;
        }

        // Resolve the requested rev (or just take HEAD's fresh value
        // post-fetch). `kata update --rev v0.2.0 pj-rust` jumps the
        // cache to v0.2.0; `kata update pj-rust` (no --rev) takes
        // whatever the default branch's HEAD now points at.
        let target = match &rev_override {
            Some(r) => r.clone(),
            None => "HEAD".to_string(),
        };
        if let Err(e) = git::checkout(slot.as_path(), &target).await {
            // Fallback: if the user-provided rev or `HEAD` can't
            // resolve in the freshly-fetched slot, surface but keep
            // going for siblings.
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

    applied.save(&pj_root)?;
    for line in report {
        println!("{line}");
    }
    Ok(())
}

fn is_local_source(s: &str) -> bool {
    s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || {
        let bytes = s.as_bytes();
        bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'/' || bytes[2] == b'\\')
    }
}

fn short_sha(s: &str) -> String {
    if s.len() >= 7 {
        s[..7].to_string()
    } else {
        s.to_string()
    }
}
