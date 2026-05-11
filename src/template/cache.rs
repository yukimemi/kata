//! On-disk cache for git-fetched template repositories.
//!
//! Layout: `<template_cache_dir>/<sha256(source)[..16]>@<rev>/`.
//! The hash collapses long URLs to a fixed-width filesystem-safe
//! name; `<rev>` (sanitised) keeps multiple versions of the same
//! source separable. Phase 2-c1 trusts the cache once a slot
//! exists; `kata update` (Phase 2-g) is the explicit refresh path.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, OnceLock};

use camino::Utf8PathBuf;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::git;
use crate::paths::template_cache_dir;

/// Per-URL serialisation locks for cache-slot mutations.
///
/// `kata update --all` / `kata apply --all` fans out PJ work via a
/// `tokio::sync::Semaphore` (default `pj_concurrency = 4`). When
/// multiple PJs share a template source URL, they hash to the **same**
/// slot under `cache/templates/<hash>/` and the parallel workers
/// race on the slot's git index. Symptoms span three flavours: two
/// `git fetch`es racing on `.git/objects/`, two fetches racing on
/// `refs/remotes/origin/<branch>`, and a `checkout` racing against
/// a fetch on `.git/index.lock`. See yukimemi/kata#35 for repro logs.
///
/// Fix: hand each URL its own `Arc<Mutex<()>>`. Workers acquire the
/// per-URL lock around the fetch + checkout pair; PJs touching
/// different URLs proceed in parallel, same-URL accesses queue.
/// The lock is **finer** than the per-PJ semaphore — concurrency is
/// preserved exactly when it's safe.
///
/// **Growth note**: the map gains an entry per distinct URL ever
/// requested in this process and never prunes. That's intentional
/// for kata's CLI execution model — a single `kata update --all`
/// touches at most a few dozen URLs and the process exits seconds
/// later. If kata ever grows a daemon mode (long-running server,
/// long-lived MCP shim, etc.), revisit with a weak-ref / LRU
/// strategy so stale entries can be reclaimed.
#[derive(Default)]
struct CacheLocks {
    // std::sync::Mutex (cheap to acquire briefly) protects the map
    // itself; the per-URL inner mutex is tokio::sync::Mutex so async
    // workers can hold it across `.await` points (clone / fetch /
    // checkout are all async).
    map: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl CacheLocks {
    fn for_url(&self, url: &str) -> Arc<Mutex<()>> {
        let mut map = self
            .map
            .lock()
            .expect("CacheLocks map mutex poisoned — a previous task panicked while inserting");
        map.entry(url.to_string()).or_default().clone()
    }
}

fn cache_locks() -> &'static CacheLocks {
    static LOCKS: OnceLock<CacheLocks> = OnceLock::new();
    LOCKS.get_or_init(CacheLocks::default)
}

/// Returns the `Arc<Mutex>` guarding cache-slot mutations for `url`.
/// Hold the resulting guard across the entire fetch / checkout
/// section that touches `cache.slot(url)`'s git index. See
/// [`CacheLocks`] for the rationale.
pub fn lock_for_url(url: &str) -> Arc<Mutex<()>> {
    cache_locks().for_url(url)
}

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
        // Serialise concurrent same-URL accesses to the slot's git
        // index. Without this, two PJs sharing a template source URL
        // race on `.git/objects/`, `refs/remotes/origin/<branch>`,
        // and `.git/index.lock`. See yukimemi/kata#35.
        let url_lock = lock_for_url(source);
        let _guard = url_lock.lock().await;
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

    #[test]
    fn lock_for_url_returns_same_mutex_for_same_url() {
        // Both callers must share an instance so the per-URL lock
        // actually serialises — if `for_url` minted a fresh mutex
        // each time, concurrent fetches on the same URL would race
        // exactly as yukimemi/kata#35 reported.
        let a = lock_for_url("github.com/yukimemi/pj-base");
        let b = lock_for_url("github.com/yukimemi/pj-base");
        assert!(
            Arc::ptr_eq(&a, &b),
            "same URL must yield identical Arc<Mutex>"
        );
    }

    #[test]
    fn lock_for_url_returns_distinct_mutex_for_different_urls() {
        // Different URLs must not contend — that's the whole point
        // of keying by URL rather than serialising globally.
        let a = lock_for_url("github.com/yukimemi/pj-base");
        let b = lock_for_url("github.com/yukimemi/pj-rust");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different URLs must yield distinct Arc<Mutex>"
        );
    }

    #[tokio::test]
    async fn lock_for_url_serialises_concurrent_holders() {
        // Two tasks contending on the same URL should observe the
        // canonical "one inside the critical section at a time"
        // behaviour. Use a shared counter that's incremented to 1
        // inside the guard, asserted equal to 1, decremented back to
        // 0 before release — if the lock didn't serialise, the
        // second task would see the counter at 1 and fail.
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::task::JoinSet;
        use tokio::time::{Duration, sleep};

        let url = "github.com/test/concurrent";
        let counter = StdArc::new(AtomicU32::new(0));
        let mut set = JoinSet::new();
        for _ in 0..4 {
            let counter = counter.clone();
            set.spawn(async move {
                let lock = lock_for_url(url);
                let _guard = lock.lock().await;
                let before = counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(before, 0, "another task was inside the guard");
                sleep(Duration::from_millis(10)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while let Some(res) = set.join_next().await {
            res.expect("task panicked — lock failed to serialise");
        }
    }
}
