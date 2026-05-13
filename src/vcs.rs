//! VCS-state inspection helpers for `kata apply --all` (#80).
//!
//! Phase 1 / 2 only ever clone template repos. Phase 3 added the
//! ability to fan out `apply` across every registered project,
//! which immediately raises the question: is each target PJ in a
//! state where overwriting template-managed files is safe? An
//! uncommitted edit could be silently clobbered, or the apply's
//! diff could be polluted with unrelated work-in-progress.
//!
//! This module wraps the lightweight side of the answer:
//!
//! - [`dirty_files`] runs `git status --porcelain` and returns the
//!   list of "user-dirty" paths — kata-owned bookkeeping files
//!   (`.kata/applied.toml`, `.kata/vars*.toml`) are filtered out
//!   so jj-colocated repos don't get flagged as dirty every time
//!   `git push` moves the upstream pointer.
//!
//! Detection limitation (out of scope for this iteration): jj
//! workspaces that aren't git-colocated are reported as non-git
//! and therefore "clean by inference". Almost every yukimemi/* PJ
//! is colocated, so the git side is enough for the common case.
//! A native `jj` backend can be slotted in later behind the same
//! `dirty_files` signature.

use camino::Utf8Path;
use tokio::process::Command;

use crate::error::{Error, Result};

/// Spawn `git` in `dir` with the C locale forced so our
/// substring-matching on stderr (`"not a git repository"`,
/// `"no tracking information"`, …) doesn't false-negative on a
/// consumer running under e.g. `LANG=ja_JP.UTF-8`. Every shell-out
/// in this module should go through this helper instead of
/// `Command::new("git")` directly so the locale guarantee can't
/// drift away in a later refactor. See PR #97 review.
fn git_in(dir: &Utf8Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir.as_std_path())
        .env("LC_ALL", "C")
        .env("LANG", "C");
    cmd
}

/// Files kata itself manages inside `<pj>/.kata/`. These are
/// filtered from the dirty-file list because:
///
/// - `.kata/applied.toml` is jj-import-from-git noise: a jj-
///   colocated repo flags it as `M` right after `git push` moves
///   the upstream pointer, even though the consumer didn't touch
///   it.
/// - `.kata/vars*.toml` is consumer-owned but kata writes the
///   initial seed during `init` and rewrites parts of it during
///   `apply --reseed`; treating it as kata's bookkeeping
///   prevents spurious "dirty" flags during a fresh seed cycle.
///
/// Anything else under `.kata/` (consumer-authored notes,
/// hand-staged experiments, etc.) is **NOT** filtered — those
/// count as real user WIP and the pre-flight check should
/// surface them like any other file.
fn is_kata_owned(path: &str) -> bool {
    if path == ".kata/applied.toml" {
        return true;
    }
    // `.kata/vars.toml` and `.kata/vars.<layer>.toml` — kata-
    // managed seeds (see #86). Nested `if let`s instead of a
    // let-chain so the check compiles on MSRV 1.85
    // (let_chains stabilised in 1.88).
    if let Some(rest) = path.strip_prefix(".kata/") {
        if let Some(stripped) = rest.strip_suffix(".toml") {
            if stripped == "vars" {
                return true;
            }
            if let Some(layer) = stripped.strip_prefix("vars.") {
                return !layer.is_empty() && !layer.contains('/');
            }
        }
    }
    false
}

/// Fast-forward `dir`'s current branch from its tracked remote
/// (#94, the `--pull` half of the `kata apply --all` ergonomics).
/// `--ff-only` is intentional: if local commits diverge from the
/// remote, the consumer's working state is in a shape kata
/// shouldn't try to auto-merge — surface the failure and let the
/// per-PJ error path report it.
///
/// Like [`dirty_files`], `Ok(None)` means "not a git repo (or no
/// upstream)" — the apply --all caller treats that as
/// "nothing to pull, fall through". Real errors (non-fast-forward,
/// network failure, `git` missing) come back as `Err`.
pub async fn pull_ff(dir: &Utf8Path) -> Result<Option<()>> {
    let output = git_in(dir)
        .args(["pull", "--ff-only"])
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git pull` in {dir}: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        // Common stderr fragments for "no upstream configured":
        //   "There is no tracking information for the current branch."
        //   "No remote repository specified."
        // Treat both as "nothing to pull" — the PJ is local-only
        // and apply should still proceed.
        if stderr.contains("no tracking information")
            || stderr.contains("No remote repository specified")
        {
            return Ok(None);
        }
        return Err(Error::Git(format!("git pull in {dir}: {}", stderr.trim())));
    }
    Ok(Some(()))
}

/// Stage the given paths and create a commit with `msg` in `dir`.
/// Returns `Ok(false)` when nothing was actually committed (every
/// listed path was already in the index without changes — the
/// usual case when the apply was a pure no-op), so callers can
/// short-circuit the follow-up `git push`.
///
/// Only the paths kata wrote in this run are staged; unrelated
/// consumer WIP in the working tree is intentionally NOT included
/// so a `--allow-dirty` apply still keeps the kata commit and
/// the consumer's WIP separate.
pub async fn commit_paths(dir: &Utf8Path, paths: &[String], msg: &str) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    // Stage each kata-touched path explicitly. `--` separates
    // paths from options the same way other shell-outs in this
    // crate do, and `git add -A -- <paths>` is intentional: `-A`
    // also picks up deletions, which can happen when a template
    // stops shipping a file. The paths list comes from kata's
    // own runner, so it's never user-attacker-controllable.
    let mut add = git_in(dir);
    add.args(["add", "-A", "--"]);
    for p in paths {
        add.arg(p);
    }
    let output = add
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git add` in {dir}: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git add in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    // `--allow-empty=false` is git's default. Detect a no-op
    // commit BEFORE invoking it so we don't return a spurious
    // success when there's nothing to record. The pathspec is
    // critical (PR #97 review): without `-- <paths>` the diff
    // inspects the entire staged index, which under
    // `--allow-dirty` would include consumer-staged files and
    // mis-classify a kata no-op as "something to commit".
    let mut cached_cmd = git_in(dir);
    cached_cmd.args(["diff", "--cached", "--quiet", "--"]);
    for p in paths {
        cached_cmd.arg(p);
    }
    let cached = cached_cmd
        .status()
        .await
        .map_err(|e| Error::Git(format!("spawn `git diff --cached` in {dir}: {e}")))?;
    if cached.success() {
        // Nothing to commit at the listed paths. Not an error —
        // apply genuinely produced no on-disk delta on the kata-
        // owned set.
        return Ok(false);
    }

    // `--only -- <paths>` (PR #97 review): commit ONLY the
    // kata-touched paths even if other entries happen to be
    // staged in the consumer's index. Combined with the pathspec
    // on the cached check above, this guarantees the commit
    // contains exactly the same set kata wrote and nothing else.
    let mut commit = git_in(dir);
    commit.args(["commit", "-m", msg, "--only", "--"]);
    for p in paths {
        commit.arg(p);
    }
    let output = commit
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git commit` in {dir}: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git commit in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(true)
}

/// Push the current branch in `dir` to its tracked upstream.
/// `Ok(false)` when no upstream is configured (the PJ is
/// local-only) so the caller can log a warning instead of
/// surfacing an error — matching `pull_ff`'s
/// "missing-upstream is not a hard failure" stance.
///
/// The "no upstream" detection is a pre-flight
/// `git rev-parse --abbrev-ref --symbolic-full-name @{u}` rather
/// than substring-matching the push's stderr. The previous
/// heuristic (`"no upstream"` / `"does not match any"`) had two
/// problems: it conflated a missing upstream with a missing
/// local branch (refspec-not-found is a real push failure, not
/// a benign skip), and the wording was locale-dependent. See
/// PR #97 review.
pub async fn push_current(dir: &Utf8Path) -> Result<bool> {
    // `@{u}` resolves to the upstream of the current branch.
    // Non-zero exit = no upstream configured (or detached HEAD,
    // which is the same situation from kata's point of view —
    // there's nothing to push to).
    let upstream = git_in(dir)
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git rev-parse @{{u}}` in {dir}: {e}")))?;
    if !upstream.status.success() {
        // No upstream / detached HEAD: kata-level "nothing to
        // push", let the caller log a warning.
        return Ok(false);
    }

    let output = git_in(dir)
        .arg("push")
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git push` in {dir}: {e}")))?;
    if !output.status.success() {
        return Err(Error::Git(format!(
            "git push in {dir}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(true)
}

/// Inspect `dir` for uncommitted user work. Returns a list of
/// relative paths the user has modified / added / removed, with
/// kata-owned bookkeeping filtered out. Empty vec means clean.
///
/// `Ok(None)` is reserved for "not a git repo (or git unavailable)" —
/// callers treat that as "no VCS to consult, fall through to apply
/// without a pre-flight veto". A real I/O error (spawn failure,
/// git crash with stderr) is `Err`.
pub async fn dirty_files(dir: &Utf8Path) -> Result<Option<Vec<String>>> {
    // `-z` switches porcelain v1 to NUL-terminated, unquoted
    // output. Without it, paths with whitespace / non-ASCII /
    // backslash characters arrive as C-style escaped quoted
    // strings (`"a\\b.rs"`, `"a\303\251.rs"`), and our cheap
    // `trim_matches('"')` would surface the still-escaped form
    // — see PR #92 review. The NUL separator also unambiguously
    // splits the two paths of a rename / copy entry, so we don't
    // need the heuristic ` -> ` scan any more.
    let output = git_in(dir)
        .args(["status", "--porcelain", "-z"])
        .output()
        .await
        .map_err(|e| Error::Git(format!("spawn `git status` in {dir}: {e}")))?;

    if !output.status.success() {
        // `git status` exits non-zero outside a repo with a stderr
        // like "fatal: not a git repository". Treat that as
        // "no VCS info available" rather than a hard error so a
        // non-git PJ doesn't block the whole `--all` run.
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(Error::Git(format!(
            "git status in {dir}: {}",
            stderr.trim()
        )));
    }

    // `String::from_utf8_lossy` accepts the embedded NUL bytes
    // just fine — they pass through as `\0` chars, which is what
    // `split('\0')` expects.
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(Some(parse_porcelain_z(&stdout)))
}

/// Parse `git status --porcelain -z` output into a list of dirty
/// paths, with kata-owned bookkeeping filtered out. Each record
/// is NUL-terminated. Rename / copy records consume a *second*
/// NUL-separated field (the original path) so the iterator must
/// advance two steps when the index status is `R` or `C`.
///
/// Public for unit testing — production code calls
/// [`dirty_files`].
pub(crate) fn parse_porcelain_z(porcelain: &str) -> Vec<String> {
    let mut out = Vec::new();
    // `split('\0')` returns an empty trailing element after the
    // final terminator; the `if entry.is_empty() { continue; }`
    // below silently drops it.
    let mut iter = porcelain.split('\0');
    while let Some(entry) = iter.next() {
        if entry.is_empty() {
            continue;
        }
        // A real entry is "XY <path>" — at minimum 2 status
        // chars + 1 space + 1 path char = 4 bytes. Anything
        // shorter is malformed; skip.
        if entry.len() < 4 {
            continue;
        }
        let xy = &entry[..2];
        let dst = &entry[3..];
        let index_status = xy.chars().next().unwrap_or(' ');
        if matches!(index_status, 'R' | 'C') {
            // Porcelain v1 -z renames / copies: "Rxx <DST>\0<ORIG>\0".
            // The destination comes FIRST inside the record (it's the
            // path that lives on disk now), and the original path
            // follows as a separate NUL-terminated field. The order
            // is the opposite of the non-`-z` ` -> ` form.
            let orig = iter.next().unwrap_or("");
            // Don't drop the record just because the destination
            // is kata-owned — if the *source* is user content
            // (e.g. `notes.md -> .kata/vars.toml`), the consumer
            // still has uncommitted user work moving into kata's
            // bookkeeping namespace, and the pre-flight check must
            // surface it. Only suppress when BOTH ends are kata
            // bookkeeping (the routine jj-colocated re-import
            // case). See PR #92 review.
            if !(is_kata_owned(dst) && is_kata_owned(orig)) {
                // Surface the destination — it's what lives on
                // disk now and what kata would clobber if it
                // tried to write that path.
                if !dst.is_empty() {
                    out.push(dst.replace('\\', "/"));
                }
            }
        } else if !is_kata_owned(dst) {
            out.push(dst.replace('\\', "/"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_and_added_lines() {
        // `-z` output: each record terminated by NUL. Trailing
        // NUL is fine — `split('\0')` drops the empty tail.
        let out = " M src/main.rs\0A  README.md\0?? scratch.txt\0";
        let mut dirty = parse_porcelain_z(out);
        dirty.sort();
        assert_eq!(
            dirty,
            vec![
                "README.md".to_string(),
                "scratch.txt".to_string(),
                "src/main.rs".to_string()
            ],
        );
    }

    #[test]
    fn filters_kata_bookkeeping() {
        // jj-colocated repos routinely show .kata/applied.toml as
        // modified right after a `git push` — that's kata-owned
        // metadata and should NOT count as user-dirty.
        let out = " M .kata/applied.toml\0 M .kata/vars.toml\0 M src/lib.rs\0";
        let dirty = parse_porcelain_z(out);
        assert_eq!(dirty, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn empty_porcelain_means_clean() {
        assert!(parse_porcelain_z("").is_empty());
        assert!(parse_porcelain_z("\0\0").is_empty());
    }

    #[test]
    fn rename_lines_use_destination() {
        // -z rename: "Rxx DST\0ORIG\0" — destination first.
        let out = "R  new/path.rs\0old/path.rs\0";
        assert_eq!(parse_porcelain_z(out), vec!["new/path.rs".to_string()],);
    }

    #[test]
    fn quoted_paths_are_no_longer_quoted_under_z() {
        // Under `-z` git emits paths verbatim with no quoting,
        // so a path with whitespace arrives without wrapping
        // double quotes.
        let out = " M file with spaces.rs\0";
        assert_eq!(
            parse_porcelain_z(out),
            vec!["file with spaces.rs".to_string()],
        );
    }

    #[test]
    fn path_containing_arrow_is_not_split() {
        // No -> substring scan any more: `-z` records are NUL-
        // separated, so a filename like `a -> b.txt` modified
        // in place stays intact regardless of status.
        let out = " M a -> b.txt\0";
        assert_eq!(parse_porcelain_z(out), vec!["a -> b.txt".to_string()],);
    }

    #[test]
    fn rename_into_kata_owned_from_user_path_still_surfaces() {
        // CodeRabbit PR #92 follow-up: a rename of a user file
        // INTO kata's bookkeeping namespace (e.g.
        // `notes.md -> .kata/vars.toml`) must NOT be silently
        // dropped — the source side is still uncommitted user
        // work, and the pre-flight check exists exactly to
        // protect that.
        let out = "R  .kata/vars.toml\0notes.md\0";
        assert_eq!(
            parse_porcelain_z(out),
            vec![".kata/vars.toml".to_string()],
            "rename whose source is user content must surface",
        );
    }

    #[test]
    fn rename_within_kata_owned_files_is_suppressed() {
        // Both sides are kata-managed bookkeeping — that's the
        // routine jj-import-from-git noise the filter is meant to
        // hide. Drop the record.
        let out = "R  .kata/applied.toml\0.kata/vars.toml\0";
        assert!(
            parse_porcelain_z(out).is_empty(),
            "rename between two kata-owned paths is routine and should be filtered",
        );
    }

    #[test]
    fn copy_status_consumes_two_paths() {
        // Same shape as R-rename: two NUL-separated paths per
        // copy entry, destination first.
        let out = "C  dest.rs\0src.rs\0";
        assert_eq!(parse_porcelain_z(out), vec!["dest.rs".to_string()]);
    }

    #[test]
    fn filters_only_intended_kata_files() {
        // `.kata/applied.toml` and `.kata/vars*.toml` are
        // filtered (kata-managed bookkeeping); consumer-authored
        // files elsewhere under `.kata/` count as real WIP.
        let out = " M .kata/applied.toml\0 M .kata/vars.toml\0 M .kata/vars.rust.toml\0 M .kata/scratch.md\0 M src/lib.rs\0";
        let mut dirty = parse_porcelain_z(out);
        dirty.sort();
        assert_eq!(
            dirty,
            vec![".kata/scratch.md".to_string(), "src/lib.rs".to_string()],
        );
    }

    #[test]
    fn path_with_inner_whitespace_preserved_under_z() {
        // No quoting under `-z`, so leading/trailing whitespace
        // inside the filename comes through verbatim. (Pre-`-z`
        // we had to use `trim_matches('"')` to keep this; with
        // `-z` the whitespace just survives.)
        let out = " M   leading.rs\0";
        // The first three bytes are "XY ", so the rest is "  leading.rs".
        assert_eq!(parse_porcelain_z(out), vec!["  leading.rs".to_string()],);
    }

    #[test]
    fn malformed_short_record_is_dropped() {
        // Anything shorter than "XY <path>" (4 bytes) is bogus
        // and silently skipped — empty records included.
        let out = "XY\0 M\0\0";
        assert!(parse_porcelain_z(out).is_empty());
    }
}
