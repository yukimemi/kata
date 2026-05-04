//! TemplateHandle — a *resolved* template, ready to be walked and
//! applied. Local sources resolve directly against `base_dir`; git
//! sources go through `TemplateCache` (clone-on-first-use into
//! `~/.cache/kata/templates/...`).

pub mod cache;
pub mod source;

use camino::{Utf8Path, Utf8PathBuf};

pub use cache::TemplateCache;
pub use source::TemplateSource;

use crate::error::{Error, Result};
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::preset::TemplateRef;

#[derive(Debug, Clone)]
pub struct TemplateHandle {
    /// The original spec (`./local/x`, `github.com/.../...`).
    pub source_spec: String,
    /// Resolved revision label. `"local"` for Local sources; commit
    /// SHA in Phase 2 for Git sources.
    pub rev: String,
    /// Sub-directory inside the source spec, preserved so it survives
    /// the round-trip through `applied.toml`.
    pub subdir: Option<String>,
    /// On-disk root the template lives in.
    pub root: Utf8PathBuf,
    /// Parsed manifest (`template.toml`).
    pub manifest: Manifest,
}

impl TemplateHandle {
    /// Resolve a `TemplateRef` to a `TemplateHandle`. Local sources
    /// land directly under `base_dir`; git sources are clone-on-
    /// first-use into `TemplateCache`. Subdir validation has already
    /// run inside `TemplateSource::from_ref`.
    pub async fn load(t: &TemplateRef, base_dir: &Utf8Path) -> Result<Self> {
        let source = TemplateSource::from_ref(t, base_dir)?;
        let (root, rev) = match source {
            TemplateSource::Local { root } => (root, "local".to_string()),
            TemplateSource::Git {
                url,
                rev: rev_spec,
                subdir,
            } => {
                let cache = TemplateCache::ensure()?;
                let (slot, sha) = cache
                    .fetch_or_clone(&url, rev_spec.as_deref())
                    .await
                    .map_err(|e| {
                        // Surface the original spec in the error rather
                        // than the normalised URL — easier to map back
                        // to what the user / preset wrote.
                        Error::template(t.source.clone(), e.to_string())
                    })?;
                let root = match subdir {
                    Some(sub) => slot.join(sub),
                    None => slot,
                };
                (root, sha)
            }
        };
        let manifest_path = root.join(MANIFEST_FILE);
        let manifest = Manifest::load(manifest_path.as_std_path())?;
        Ok(Self {
            source_spec: t.source.clone(),
            rev,
            subdir: t.subdir.clone(),
            root,
            manifest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn loads_a_local_template() {
        let td = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();
        let mut f = std::fs::File::create(root.join(MANIFEST_FILE)).unwrap();
        writeln!(
            f,
            r#"
            name = "demo"
            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            "#
        )
        .unwrap();

        let t = TemplateRef {
            source: root.as_str().to_string(),
            rev: None,
            subdir: None,
        };
        let h = futures_runtime(async { TemplateHandle::load(&t, Utf8Path::new(".")).await });
        let h = h.unwrap();
        assert_eq!(h.manifest.name, "demo");
        assert_eq!(h.rev, "local");
    }

    // Git source loading is exercised end-to-end in
    // `tests/git_source.rs` (it requires a real git CLI + tempdir
    // bare-repo fixture, which is awkward inside a unit test).

    /// Tiny single-thread runtime for sync tests of async APIs.
    fn futures_runtime<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(f)
    }
}
