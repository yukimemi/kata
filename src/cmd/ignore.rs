//! `kata ignore <path>...` / `kata unignore <path>...` — tell kata a
//! template-managed file is intentionally absent.
//!
//! `ignore` records `ignored = true` for the file in
//! `.kata/applied.toml` and deletes it from disk (after confirmation
//! unless `--yes`). From then on `kata apply` neither writes nor
//! recreates it, whatever its `how` / `when`, and `--force` /
//! `--reseed` do not override the marker. `unignore` clears the marker
//! only; the next `kata apply` follows the file's normal `when` rules
//! again (an `always` file comes back; a `once` file that was already
//! applied still needs `--reseed`).
//!
//! A path must already be known to kata: part of the current plan or
//! recorded in `applied.toml`. A file kata has never seen is rejected
//! so a typo cannot silently create a marker.

use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};

use crate::applied::AppliedState;
use crate::config::ProjectEntry;
use crate::error::{Error, Result};
use crate::preset::TemplateRef;
use crate::runner::{check_relative_contained, normalize_relative_path, plan_pj};

use super::resolve_pj_root;

fn locate(at: Option<Utf8PathBuf>) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;
    Ok((cwd, pj_root))
}

/// Turn a user-typed path into the `applied.toml` key form
/// (relative to the PJ root, `/`-separated, normalised). Absolute
/// paths inside the PJ root are accepted and made relative.
fn to_key(pj_root: &Utf8Path, raw: &str) -> Result<String> {
    let mut rel = raw.to_string();
    if Utf8Path::new(raw).is_absolute() {
        rel = Utf8Path::new(raw)
            .strip_prefix(pj_root)
            .map_err(|_| Error::Config(format!("`{raw}` is outside the project root {pj_root}")))?
            .to_string();
    }
    check_relative_contained(&rel, "path")?;
    let key = normalize_relative_path(&rel);
    if key.is_empty() {
        return Err(Error::Config(format!("`{raw}` is not a file path")));
    }
    if key == ".kata/applied.toml" {
        return Err(Error::Config(
            "`.kata/applied.toml` is kata's own state and cannot be ignored".into(),
        ));
    }
    Ok(key)
}

/// Destinations the current templates would manage, so a path that
/// has never been recorded (a first-time file) can still be ignored.
/// Best effort: if the plan cannot be computed, only recorded keys
/// are accepted.
async fn plan_destinations(
    pj_root: &Utf8Path,
    cwd: &Utf8Path,
    applied: &AppliedState,
) -> BTreeSet<String> {
    let templates: Vec<TemplateRef> = applied
        .templates
        .iter()
        .map(|t| TemplateRef {
            source: t.source.clone(),
            rev: Some(t.rev.clone()),
            subdir: t.subdir.clone(),
        })
        .collect();
    if templates.is_empty() {
        return BTreeSet::new();
    }
    let project = ProjectEntry {
        name: pj_root.file_name().unwrap_or("kata-project").to_string(),
        path: pj_root.to_path_buf(),
        tags: vec![],
        overrides: None,
    };
    let base_dir = applied
        .base_dir
        .clone()
        .unwrap_or_else(|| cwd.to_path_buf());
    match plan_pj(
        project,
        pj_root.to_path_buf(),
        templates,
        base_dir,
        toml::Table::new(),
        false,
        Default::default(),
    )
    .await
    {
        Ok(plans) => plans
            .into_iter()
            .map(|(dst, _, _)| normalize_relative_path(&dst))
            .filter(|d| !d.starts_with("[repo] "))
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// Refuse anything that is not a regular file or a leaf symlink, and
/// anything reached through a symlinked parent directory (deleting
/// through it could leave the project root).
fn removable(pj_root: &Utf8Path, key: &str) -> Result<bool> {
    let abs = pj_root.join(key);
    let mut cur = pj_root.to_path_buf();
    let comps: Vec<&str> = key.split('/').collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        let Ok(md) = cur.as_std_path().symlink_metadata() else {
            return Ok(false);
        };
        if i + 1 < comps.len() && !md.is_dir() {
            return Err(Error::Config(format!(
                "`{key}` is reached through a symlink or non-directory; refusing to delete"
            )));
        }
    }
    let md = abs
        .as_std_path()
        .symlink_metadata()
        .map_err(|e| Error::io_at(abs.as_std_path(), e))?;
    if md.is_dir() {
        return Err(Error::Config(format!(
            "`{key}` is a directory; `kata ignore` takes files"
        )));
    }
    Ok(true)
}

pub async fn run(
    paths: Vec<String>,
    at: Option<Utf8PathBuf>,
    yes: bool,
    non_interactive: bool,
    _no_color: bool,
) -> Result<()> {
    let (cwd, pj_root) = locate(at)?;
    let mut applied = AppliedState::load(&pj_root)?;

    let keys: Vec<String> = paths
        .iter()
        .map(|p| to_key(&pj_root, p))
        .collect::<Result<_>>()?;

    let known = plan_destinations(&pj_root, &cwd, &applied).await;
    for (raw, key) in paths.iter().zip(&keys) {
        if !known.contains(key) && !applied.files.contains_key(key) {
            return Err(Error::Config(format!(
                "`{raw}` is not a file kata manages here (not in the current templates \
                 and not recorded in applied.toml); run `kata apply` first if it has \
                 never been written"
            )));
        }
    }

    // Validate every deletion up front so a bad path in the middle of
    // the list does not leave a half-processed batch.
    let mut present = Vec::with_capacity(keys.len());
    for key in &keys {
        present.push(removable(&pj_root, key)?);
    }

    if !yes && present.iter().any(|p| *p) {
        if non_interactive {
            return Err(Error::Config(
                "`kata ignore` deletes files; pass --yes to confirm in non-interactive mode".into(),
            ));
        }
        let list: Vec<&str> = keys
            .iter()
            .zip(&present)
            .filter(|(_, p)| **p)
            .map(|(k, _)| k.as_str())
            .collect();
        let ok = inquire::Confirm::new(&format!(
            "Delete {} and stop kata from managing it?",
            list.join(", ")
        ))
        .with_default(false)
        .prompt()
        .map_err(|e| match e {
            inquire::InquireError::OperationCanceled
            | inquire::InquireError::OperationInterrupted => Error::Cancelled,
            other => Error::Other(anyhow::anyhow!("prompt: {other}")),
        })?;
        if !ok {
            println!("aborted; nothing changed");
            return Ok(());
        }
    }

    // Marker first, deletion second: a failed delete leaves "ignored
    // but present", which apply handles safely; the reverse order
    // could lose the marker and have apply recreate the file.
    for key in &keys {
        let mut fs = applied.files.get(key).cloned().unwrap_or_default();
        fs.ignored = true;
        fs.content_hash = None;
        applied.record(key, fs);
    }
    applied.save(&pj_root)?;

    for (key, was_present) in keys.iter().zip(&present) {
        if *was_present {
            let abs = pj_root.join(key);
            std::fs::remove_file(abs.as_std_path())
                .map_err(|e| Error::io_at(abs.as_std_path(), e))?;
            println!("ignored and deleted {key}");
        } else {
            println!("ignored {key} (already absent)");
        }
    }
    Ok(())
}

pub fn run_unignore(paths: Vec<String>, at: Option<Utf8PathBuf>) -> Result<()> {
    let (_cwd, pj_root) = locate(at)?;
    let mut applied = AppliedState::load(&pj_root)?;

    let keys: Vec<String> = paths
        .iter()
        .map(|p| to_key(&pj_root, p))
        .collect::<Result<_>>()?;
    for (raw, key) in paths.iter().zip(&keys) {
        if !applied.files.get(key).is_some_and(|s| s.ignored) {
            return Err(Error::Config(format!("`{raw}` is not ignored")));
        }
    }
    for key in &keys {
        if let Some(fs) = applied.files.get_mut(key) {
            fs.ignored = false;
            if fs.is_empty() {
                applied.files.remove(key);
            }
        }
    }
    applied.save(&pj_root)?;
    for key in &keys {
        println!("unignored {key}; the next `kata apply` manages it again");
    }
    Ok(())
}
