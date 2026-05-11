//! On-disk cache for git-fetched template repositories.
//!
//! Layout: `<template_cache_dir>/<sha256(source)[..16]>@<rev>/`.
//! The hash collapses long URLs to a fixed-width filesystem-safe
//! name; `<rev>` (sanitised) keeps multiple versions of the same
//! source separable. Phase 2-c1 trusts the cache once a slot
//! exists; `kata update` (Phase 2-g) is the explicit refresh path.

use std::fmt::Write;

use camino::Utf8PathBuf;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::git;
use crate::paths::template_cache_dir;

pub struct TemplateCache {
    pub root: Utf8PathBuf,
}

impl TemplateCache {
    /// Resolve and create the cache root directory. Idempotent.
    pub fn ensure() -> Result<Self> {
        let root = template_cache_dir()?;
        std::fs::create_dir_all(root.as_std_path())
            .map_err(|e| Error::io_at(root.as_std_path(), e))?;
        Ok(Self { root })
    }

    /// Slot path for a given source URL. **Source-only** — multiple
    /// revs of the same source share one working tree, re-checked-
    /// out on demand. Keeps re-apply cheap (no re-clone when only
    /// the rev label changes) at the cost of refusing parallel
    /// multi-rev apply against the same source. Phase 2-c1 doesn't
    /// have parallel apply yet, so the trade is a non-issue.
    pub fn slot(&self, source: &str) -> Utf8PathBuf {
        let mut h = Sha256::new();
        h.update(source.as_bytes());
        let bytes = h.finalize();
        let mut hex = String::with_capacity(16);
        for b in bytes.iter().take(8) {
            let _ = write!(hex, "{b:02x}");
        }
        self.root.join(hex)
    }

    /// Make sure the slot for `source` exists and is checked out at
    /// `rev_spec` (or `origin/HEAD` — the remote's default branch
    /// tip — when no rev is supplied). Returns `(slot path,
    /// resolved commit SHA)`.
    ///
    /// When the slot is already cached, this performs `git fetch
    /// --prune` first, then the checkout. Without that refresh, a
    /// long-lived cache slot stays frozen at whatever SHA the most
    /// recent operation left it at, and any upstream additions
    /// (new preset files, new template files) are invisible — fails
    /// downstream with a confusing "file not found" against the
    /// cache path. See yukimemi/kata#33.
    ///
    /// `origin/HEAD` (the symref `git clone` sets up pointing at
    /// the remote's default branch tip) is the default checkout
    /// target because plain `HEAD` on a detached-HEAD cache slot
    /// is a no-op after fetch.
    pub async fn fetch_or_clone(
        &self,
        source: &str,
        rev_spec: Option<&str>,
    ) -> Result<(Utf8PathBuf, String)> {
        let slot = self.slot(source);
        // Treat the slot as cached only when it actually contains a
        // git repository. A bare `slot.exists()` would happily reuse
        // a directory left over from an interrupted clone (no
        // `.git/` inside), causing the `git checkout` below to fail
        // with a confusing "not a git repository". Recover by
        // wiping and re-cloning.
        let cached = slot.join(".git").is_dir();
        if !cached {
            if slot.exists() {
                std::fs::remove_dir_all(slot.as_std_path())
                    .map_err(|e| Error::io_at(slot.as_std_path(), e))?;
            }
            if let Some(parent) = slot.parent() {
                std::fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
            }
            git::clone_at(source, slot.as_path()).await?;
        } else {
            // Best-effort refresh so any upstream additions since
            // the last operation against this slot become visible.
            // Failure here is non-fatal — an offline user with a
            // hot cache can still proceed against existing local
            // refs. The subsequent checkout surfaces the real
            // problem if the requested rev is missing locally.
            if let Err(e) = git::fetch(slot.as_path()).await {
                eprintln!("kata: warning: fetch failed for {source}: {e}; using cached refs");
            }
        }
        // `applied.toml` may carry either a symbolic rev (branch /
        // tag) from the original spec or a resolved commit SHA from
        // a previous apply — both are valid checkout targets. When
        // unspecified, advance to the remote's default branch tip;
        // see the doc comment for why `origin/HEAD` and not `HEAD`.
        let target = rev_spec.unwrap_or("origin/HEAD");
        git::checkout(slot.as_path(), target).await?;
        let head = git::current_head(slot.as_path()).await?;
        Ok((slot, head))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_is_stable_for_same_source() {
        let cache = TemplateCache {
            root: Utf8PathBuf::from("/tmp/kata-cache"),
        };
        let a = cache.slot("github.com/yukimemi/pj-base");
        let b = cache.slot("github.com/yukimemi/pj-base");
        assert_eq!(a, b);
    }

    #[test]
    fn slot_is_invariant_to_rev() {
        // Source-only key by design: multiple revs of the same
        // source share one working tree (re-checked-out on demand).
        let cache = TemplateCache {
            root: Utf8PathBuf::from("/tmp/kata-cache"),
        };
        let s = cache.slot("github.com/x/y");
        let s_again = cache.slot("github.com/x/y");
        assert_eq!(s, s_again);
    }

    #[test]
    fn slot_differs_for_different_sources() {
        let cache = TemplateCache {
            root: Utf8PathBuf::from("/tmp/kata-cache"),
        };
        let a = cache.slot("github.com/x/a");
        let b = cache.slot("github.com/x/b");
        assert_ne!(a, b);
    }
}
