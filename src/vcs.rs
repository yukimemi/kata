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

/// Inspect `dir` for uncommitted user work. Returns a list of
/// relative paths the user has modified / added / removed, with
/// kata-owned bookkeeping filtered out. Empty vec means clean.
///
/// `Ok(None)` is reserved for "not a git repo (or git unavailable)" —
/// callers treat that as "no VCS to consult, fall through to apply
/// without a pre-flight veto". A real I/O error (spawn failure,
/// git crash with stderr) is `Err`.
pub async fn dirty_files(dir: &Utf8Path) -> Result<Option<Vec<String>>> {
    let output = Command::new("git")
        .current_dir(dir.as_std_path())
        .args(["status", "--porcelain"])
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

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(Some(parse_porcelain(&stdout)))
}

/// Parse `git status --porcelain` (v1) output into a list of
/// dirty paths, with kata-owned prefixes filtered out. Public for
/// unit testing — production code should call [`dirty_files`].
pub(crate) fn parse_porcelain(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter_map(parse_porcelain_line)
        .filter(|p| !is_kata_owned(p))
        .collect()
}

/// Pull the path out of one porcelain v1 line. Format is two
/// status chars + space + path.
///
/// `git status --porcelain` only emits ` -> ` between two paths
/// when the index status (col 1) is `R` (rename) or `C` (copy);
/// for every other status the path can legitimately contain that
/// substring (`a -> b.txt` is a valid filename, modified in
/// place). So the rename/copy split must be gated on the status
/// code rather than blindly searching for ` -> ` everywhere.
fn parse_porcelain_line(line: &str) -> Option<String> {
    // Minimum porcelain line: 2 status chars + 1 space + 1+ char path.
    if line.len() < 4 {
        return None;
    }
    let xy = &line[..2];
    let rest = &line[3..];
    // The index status (col 1) is what signals rename / copy
    // (`Rxxx -> yyy`); the worktree status (col 2) doesn't.
    let index_status = xy.chars().next().unwrap_or(' ');
    let path = if matches!(index_status, 'R' | 'C') {
        match rest.find(" -> ") {
            Some(arrow_at) => &rest[arrow_at + 4..],
            None => rest, // malformed, but better than dropping the line
        }
    } else {
        rest
    };
    // `trim_matches('"')` instead of `trim()` so a valid filename
    // whose leading or trailing whitespace was preserved by git's
    // quoting (`"  spaced.rs"`) survives. git quotes any path with
    // funny characters, so leading/trailing whitespace inside the
    // quotes is part of the actual filename.
    let trimmed = path.trim_matches('"');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.replace('\\', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_and_added_lines() {
        let out = " M src/main.rs\nA  README.md\n?? scratch.txt\n";
        let mut dirty = parse_porcelain(out);
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
        let out = " M .kata/applied.toml\n M .kata/vars.toml\n M src/lib.rs\n";
        let dirty = parse_porcelain(out);
        assert_eq!(dirty, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn empty_porcelain_means_clean() {
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("\n\n").is_empty());
    }

    #[test]
    fn rename_lines_use_destination() {
        let out = "R  old/path.rs -> new/path.rs\n";
        assert_eq!(parse_porcelain(out), vec!["new/path.rs".to_string()]);
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        // git quotes paths with funny characters in porcelain output.
        let out = " M \"file with spaces.rs\"\n";
        assert_eq!(
            parse_porcelain(out),
            vec!["file with spaces.rs".to_string()],
        );
    }

    #[test]
    fn modified_path_containing_arrow_is_not_treated_as_rename() {
        // Gemini PR #92 finding: `a -> b.txt` is a perfectly valid
        // filename, modified in place. The rename split must only
        // apply when the index status is `R`/`C`, otherwise we'd
        // silently drop the prefix and report `b.txt` as the dirty
        // path.
        let out = " M a -> b.txt\n";
        assert_eq!(parse_porcelain(out), vec!["a -> b.txt".to_string()]);
    }

    #[test]
    fn rename_lines_only_split_on_r_or_c_status() {
        // R = rename in index → split.
        assert_eq!(
            parse_porcelain("R  old.rs -> new.rs\n"),
            vec!["new.rs".to_string()],
        );
        // C = copy in index → split.
        assert_eq!(
            parse_porcelain("C  src.rs -> dest.rs\n"),
            vec!["dest.rs".to_string()],
        );
    }

    #[test]
    fn worktree_only_status_does_not_split_on_arrow() {
        // Worktree-only status (space in col 1) plus `R`/`C` in
        // col 2 isn't a rename — only the index status matters for
        // the porcelain v1 format.
        let out = " M weird name -> still weird.rs\n";
        assert_eq!(
            parse_porcelain(out),
            vec!["weird name -> still weird.rs".to_string()],
        );
    }

    #[test]
    fn filters_only_intended_kata_files() {
        // CodeRabbit PR #92 finding: `.kata/applied.toml` and
        // `.kata/vars*.toml` are filtered (kata-managed
        // bookkeeping), but consumer-authored files elsewhere
        // under `.kata/` count as real WIP.
        let out = " M .kata/applied.toml\n M .kata/vars.toml\n M .kata/vars.rust.toml\n M .kata/scratch.md\n M src/lib.rs\n";
        let mut dirty = parse_porcelain(out);
        dirty.sort();
        assert_eq!(
            dirty,
            vec![".kata/scratch.md".to_string(), "src/lib.rs".to_string()],
        );
    }

    #[test]
    fn quoted_path_with_inner_whitespace_is_preserved() {
        // Gemini PR #92 finding: previously `.trim()` would strip
        // leading/trailing whitespace that was meaningful inside
        // the quotes. With `trim_matches('"')` it survives.
        let out = " M \"  leading.rs\"\n";
        assert_eq!(parse_porcelain(out), vec!["  leading.rs".to_string()]);
    }
}
