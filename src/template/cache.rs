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
    /// `rev_spec` (default `HEAD` of the cloned default branch).
    /// Returns `(slot path, resolved commit SHA)`.
    pub async fn fetch_or_clone(
        &self,
        source: &str,
        rev_spec: Option<&str>,
    ) -> Result<(Utf8PathBuf, String)> {
        let slot = self.slot(source);
        if !slot.exists() {
            if let Some(parent) = slot.parent() {
                std::fs::create_dir_all(parent.as_std_path())
                    .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
            }
            git::clone_at(source, slot.as_path()).await?;
        }
        if let Some(rev) = rev_spec {
            // `applied.toml` may carry either a symbolic rev (branch /
            // tag) from the original spec or a resolved commit SHA
            // from a previous apply — both are valid checkout targets.
            git::checkout(slot.as_path(), rev).await?;
        }
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
