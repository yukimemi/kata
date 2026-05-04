//! TemplateHandle — a *resolved* template, ready to be walked and
//! applied. For Phase 1 only local templates are loadable; the API
//! is async to avoid churn when Git fetch lands in Phase 2.

pub mod source;

use camino::{Utf8Path, Utf8PathBuf};

pub use source::TemplateSource;

use crate::error::Result;
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::preset::TemplateRef;

#[derive(Debug, Clone)]
pub struct TemplateHandle {
    /// The original spec (`./local/x`, `github.com/.../...`).
    pub source_spec: String,
    /// Resolved revision label. `"local"` for Local sources; commit
    /// SHA in Phase 2 for Git sources.
    pub rev: String,
    /// On-disk root the template lives in.
    pub root: Utf8PathBuf,
    /// Parsed manifest (`template.toml`).
    pub manifest: Manifest,
}

impl TemplateHandle {
    /// Resolve a `TemplateRef` to a `TemplateHandle`. Phase 1 only
    /// supports local sources; remote sources error early with a
    /// clear message.
    pub async fn load(t: &TemplateRef, base_dir: &Utf8Path) -> Result<Self> {
        let source = TemplateSource::from_ref(t, base_dir)?;
        let root = source.require_local()?.to_path_buf();
        let manifest_path = root.join(MANIFEST_FILE);
        let manifest = Manifest::load(manifest_path.as_std_path())?;
        Ok(Self {
            source_spec: t.source.clone(),
            rev: source.rev_label(),
            root,
            manifest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
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

    #[test]
    fn errors_on_git_source() {
        let t = TemplateRef {
            source: "github.com/x/y".into(),
            rev: None,
            subdir: None,
        };
        let res = futures_runtime(async { TemplateHandle::load(&t, Utf8Path::new(".")).await });
        let err = res.unwrap_err();
        assert!(matches!(err, Error::Template { .. }));
    }

    /// Tiny single-thread runtime for sync tests of async APIs.
    fn futures_runtime<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(f)
    }
}
