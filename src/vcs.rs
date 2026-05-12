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

/// Paths kata considers its own bookkeeping. These never count as
/// "user dirty" — they exist precisely so kata can record state,
/// and jj-colocated repos routinely show `.kata/applied.toml` as
/// `M` right after a `git push` (jj's import-from-git effect)
/// even though the consumer hasn't touched it.
const KATA_OWNED_PREFIXES: &[&str] = &[".kata/"];

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
/// status chars + space + path (rename lines use ` -> ` but the
/// destination is the relevant edit).
fn parse_porcelain_line(line: &str) -> Option<String> {
    // Minimum porcelain line: 2 status chars + 1 space + 1+ char path.
    if line.len() < 4 {
        return None;
    }
    let rest = &line[3..];
    // Renames / copies show "A -> B" — the destination is the
    // edit that lives on disk now, that's what matters.
    let path = if let Some(arrow_at) = rest.find(" -> ") {
        &rest[arrow_at + 4..]
    } else {
        rest
    };
    let trimmed = path.trim().trim_matches('"');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.replace('\\', "/"))
    }
}

fn is_kata_owned(path: &str) -> bool {
    KATA_OWNED_PREFIXES.iter().any(|p| path.starts_with(p))
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
}
