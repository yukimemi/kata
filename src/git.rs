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
///
/// Branch-name resolution: kata's template cache slots are cloned
/// in detached-HEAD state and **never create local branches**, so
/// a literal `git checkout main` fails with "no such ref" the
/// moment the user asks `kata update --rev main`. To handle that
/// while still supporting branches whose names contain `/`
/// (`feature/foo`, `release/v1`), the strategy is:
///
/// 1. Try the literal `git checkout <rev>` first. SHAs, tags,
///    `HEAD`, already-qualified refs, and locally-tracked
///    branches all succeed here.
/// 2. If that fails AND the rev isn't already fully qualified
///    (i.e. doesn't start with `origin/` or `refs/`, isn't
///    `HEAD`), retry against `git checkout origin/<rev>`. This
///    rescues plain branch names whether or not they contain `/`.
pub async fn checkout(dir: &Utf8Path, rev: &str) -> Result<()> {
    if rev.starts_with('-') {
        return Err(Error::Git(format!(
            "rev `{rev}` starts with '-' (looks like a CLI option); refusing to pass to git checkout"
        )));
    }

    let literal_err = match try_checkout(dir, rev).await {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };

    // Already fully qualified? Surface the original error rather
    // than constructing an `origin/origin/...` chain.
    if rev == "HEAD" || rev.starts_with("origin/") || rev.starts_with("refs/") {
        return Err(literal_err);
    }

    let upstream = format!("origin/{rev}");
    match try_checkout(dir, &upstream).await {
        Ok(()) => Ok(()),
        // The upstream retry failed too. The literal error is the
        // more informative one to surface — it's what the user
        // actually asked for.
        Err(_) => Err(literal_err),
    }
}

async fn try_checkout(dir: &Utf8Path, rev: &str) -> Result<()> {
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

/// Derive the upstream repo basename from `git config --get
/// remote.origin.url` for the project at `dir`. Used by the cmd
/// layer to set `project.name` so it's stable across worktrees
/// instead of being the worktree directory's leaf — running
/// `kata apply` from `~/wt/<repo>/<branch>/` should still report
/// `project.name = <repo>`, not `<branch>`.
///
/// Returns `None` when the directory isn't a git repo, has no
/// `remote.origin`, or the URL doesn't end with a parseable
/// segment. Callers fall back to the directory leaf in that case.
pub async fn repo_name_from_remote(dir: &Utf8Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir.as_std_path())
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    parse_repo_basename(url.trim())
}

/// Pull the trailing repo segment out of a git remote URL. Handles
/// the four common shapes:
///
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo`
/// - `git@github.com:owner/repo.git`
/// - `git@github.com:owner/repo`
///
/// Returns `None` for empty / `/` / `:` only inputs.
fn parse_repo_basename(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // SSH URLs put the path after `:`; HTTPS / file URLs use `/`.
    // Splitting on either is enough because the trailing segment
    // has no `:` or `/` either way.
    let last = url.rsplit(['/', ':']).next()?;
    let trimmed = last.trim_end_matches(".git").trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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

#[cfg(test)]
mod tests {
    use super::parse_repo_basename;

    #[test]
    fn parse_repo_basename_handles_https_with_dot_git() {
        assert_eq!(
            parse_repo_basename("https://github.com/yukimemi/kata.git").as_deref(),
            Some("kata"),
        );
    }

    #[test]
    fn parse_repo_basename_handles_https_without_dot_git() {
        assert_eq!(
            parse_repo_basename("https://github.com/yukimemi/kata").as_deref(),
            Some("kata"),
        );
    }

    #[test]
    fn parse_repo_basename_handles_ssh_with_dot_git() {
        assert_eq!(
            parse_repo_basename("git@github.com:yukimemi/kata.git").as_deref(),
            Some("kata"),
        );
    }

    #[test]
    fn parse_repo_basename_handles_ssh_without_dot_git() {
        assert_eq!(
            parse_repo_basename("git@github.com:yukimemi/kata").as_deref(),
            Some("kata"),
        );
    }

    #[test]
    fn parse_repo_basename_returns_none_on_garbage_input() {
        assert!(parse_repo_basename("").is_none());
        assert!(parse_repo_basename("/").is_none());
        assert!(parse_repo_basename(":").is_none());
        assert!(parse_repo_basename(".git").is_none());
    }
}
