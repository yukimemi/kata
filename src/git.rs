//! Thin shell-out wrappers around the `git` CLI. Phase 2 chose
//! shell-out over libgit2 / gix because Windows linking pain
//! outweighed the benefits (yui experience). The only mandatory
//! dependency is a working `git` on `PATH`; `kata doctor` checks.

use camino::Utf8Path;
use tokio::process::Command;

use crate::error::{Error, Result};

/// Full-history clone of `url` into `dest`. Phase 2-c1 keeps the
/// whole history so any rev (branch / tag / SHA) can be checked
/// out later without a re-fetch. Shallow clones can be added
/// behind a flag if first-clone latency becomes a real complaint.
///
/// `--` separates options from positional args so a hostile preset
/// can't sneak `url = "--upload-pack=evil"` through and turn the
/// shell-out into arbitrary code execution. Same trick we use for
/// any subsequent `git` calls that take user-supplied refs.
pub async fn clone_at(url: &str, dest: &Utf8Path) -> Result<()> {
    let output = Command::new("git")
        .arg("clone")
        .arg("--")
        .arg(url)
        .arg(dest.as_str())
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git clone {url}`: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git clone {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// `git fetch --prune` inside `dir` to pull new commits + delete
/// stale remote-tracking refs. Used by `kata update` to refresh
/// the cache slot before re-checking out.
pub async fn fetch(dir: &Utf8Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(dir.as_std_path())
        .arg("fetch")
        .arg("--prune")
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git fetch`: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git fetch in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// `git checkout <rev>` inside `dir`. Suppresses git's
/// detached-HEAD chatter so kata's own log stays clean.
///
/// Note: we do **not** wrap the rev in `--`. For `git checkout`
/// specifically, `--` separates revs (left of it) from paths
/// (right), so `git checkout -- <rev>` would try to interpret
/// `<rev>` as a file path and fail. Defence in depth instead:
/// refuse revs that look like CLI options up front.
pub async fn checkout(dir: &Utf8Path, rev: &str) -> Result<()> {
    if rev.starts_with('-') {
        return Err(Error::Git(format!(
            "rev `{rev}` starts with '-' (looks like a CLI option); refusing to pass to git checkout"
        )));
    }
    let output = Command::new("git")
        .current_dir(dir.as_std_path())
        .arg("-c")
        .arg("advice.detachedHead=false")
        .arg("checkout")
        .arg(rev)
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git checkout {rev}`: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git checkout {rev} in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Resolve a rev (branch / tag / SHA / `HEAD`) to a full commit SHA
/// inside `dir`.
pub async fn rev_parse(dir: &Utf8Path, rev: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir.as_std_path())
        .arg("rev-parse")
        .arg(rev)
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git rev-parse {rev}`: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git rev-parse {rev} in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Current `HEAD` SHA in `dir`. Convenience over `rev_parse(dir, "HEAD")`.
pub async fn current_head(dir: &Utf8Path) -> Result<String> {
    rev_parse(dir, "HEAD").await
}

/// True if `git` is on PATH and runnable. Used by `kata doctor`.
pub async fn is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
