use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use crate::preset::TemplateRef;

/// Where a template lives. Phase 1 supports `Local` only; `Git`
/// is recognised so we can give a clear "not yet implemented" error.
#[derive(Debug, Clone)]
pub enum TemplateSource {
    Local {
        /// Resolved absolute (or normalised) directory.
        root: Utf8PathBuf,
    },
    Git {
        url: String,
        rev: Option<String>,
        subdir: Option<String>,
    },
}

impl TemplateSource {
    /// Classify a `TemplateRef` and resolve `Local` sources against
    /// `base_dir` (typically the directory of the preset file, or
    /// the cwd when no preset is in play).
    pub fn from_ref(t: &TemplateRef, base_dir: &Utf8Path) -> Result<Self> {
        let s = t.source.trim();
        if is_local_path(s) {
            let root_only = if Utf8Path::new(s).is_absolute() {
                Utf8PathBuf::from(s)
            } else {
                base_dir.join(s)
            };
            // Validate `subdir` BEFORE joining: absolute subdirs would
            // silently replace `root_only` (Path::join semantics), and
            // `..` segments could escape above the source root.
            // Both let a malicious / buggy preset point us at an
            // unrelated directory, so refuse them up front.
            let normalised_root = normalise(&root_only);
            let final_root = if let Some(sub) = &t.subdir {
                let sub_path = Utf8Path::new(sub);
                if sub_path.is_absolute() {
                    return Err(Error::template(
                        s,
                        format!("subdir `{sub}` must be relative, not absolute"),
                    ));
                }
                if escapes_via_parent(sub_path) {
                    return Err(Error::template(
                        s,
                        format!("subdir `{sub}` escapes the source root via `..`"),
                    ));
                }
                normalise(&normalised_root.join(sub))
            } else {
                normalised_root
            };
            Ok(Self::Local { root: final_root })
        } else {
            Ok(Self::Git {
                url: s.to_string(),
                rev: t.rev.clone(),
                subdir: t.subdir.clone(),
            })
        }
    }

    /// Stable label for `applied.toml.templates[].rev`. Local sources
    /// always use `"local"` (their content is the truth).
    pub fn rev_label(&self) -> String {
        match self {
            Self::Local { .. } => "local".to_string(),
            Self::Git { rev, .. } => rev.clone().unwrap_or_else(|| "main".to_string()),
        }
    }
}

fn is_local_path(s: &str) -> bool {
    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        // Windows drive-letter path
        return true;
    }
    false
}

/// Resolve `..` and `.` segments in a logical path. Does NOT touch
/// the filesystem (no symlink resolution, no existence check).
///
/// For absolute paths, `..` at the root is clamped (i.e. `/..` →
/// `/`); for relative paths, leading `..` segments are preserved
/// since they're meaningful relative to the cwd.
fn normalise(p: &Utf8Path) -> Utf8PathBuf {
    use camino::Utf8Component;
    let mut out = Utf8PathBuf::new();
    for comp in p.components() {
        match comp {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                // `Utf8PathBuf::pop()` correctly refuses to pop
                // above the root of an absolute path. For relative
                // paths it returns false when nothing's left to
                // pop, in which case we keep the `..` literal.
                if !out.pop() && !p.is_absolute() {
                    out.push("..");
                }
            }
            other => out.push(other.as_str()),
        }
    }
    if out.as_str().is_empty() {
        out.push(".");
    }
    out
}

/// True when the relative path `..`-pops above its starting depth at
/// any point during traversal. Pure logical check; the path may not
/// exist on disk.
fn escapes_via_parent(p: &Utf8Path) -> bool {
    use camino::Utf8Component;
    let mut depth: i32 = 0;
    for comp in p.components() {
        match comp {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Utf8Component::Normal(_) => depth += 1,
            // Absolute / drive components shouldn't reach here —
            // callers screen them earlier — but be conservative.
            Utf8Component::RootDir | Utf8Component::Prefix(_) => return true,
        }
    }
    false
}

impl TemplateSource {
    /// Phase-1 helper: extract the local root, returning an error if
    /// the source is `Git`.
    pub fn require_local(&self) -> Result<&Utf8Path> {
        match self {
            Self::Local { root } => Ok(root.as_path()),
            Self::Git { url, .. } => Err(Error::template(
                url.clone(),
                "git templates are not supported in Phase 1",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(source: &str) -> TemplateRef {
        TemplateRef {
            source: source.into(),
            rev: None,
            subdir: None,
        }
    }

    #[test]
    fn classifies_relative_local() {
        let s = TemplateSource::from_ref(&r("./local/x"), Utf8Path::new("/base")).unwrap();
        assert!(matches!(s, TemplateSource::Local { .. }));
        assert_eq!(s.rev_label(), "local");
    }

    #[test]
    fn classifies_absolute_local() {
        let s = TemplateSource::from_ref(&r("/abs/x"), Utf8Path::new("/base")).unwrap();
        assert!(matches!(s, TemplateSource::Local { .. }));
    }

    #[test]
    fn classifies_remote() {
        let s = TemplateSource::from_ref(&r("github.com/x/y"), Utf8Path::new("/base")).unwrap();
        assert!(matches!(s, TemplateSource::Git { .. }));
    }

    #[test]
    fn require_local_errors_on_git() {
        let s = TemplateSource::from_ref(&r("github.com/x/y"), Utf8Path::new("/base")).unwrap();
        let err = s.require_local().unwrap_err();
        assert!(matches!(err, Error::Template { .. }));
    }

    #[test]
    fn rejects_absolute_subdir() {
        let mut t = r("./template");
        t.subdir = Some("/etc".into());
        let err = TemplateSource::from_ref(&t, Utf8Path::new("/base")).unwrap_err();
        assert!(matches!(err, Error::Template { .. }));
    }

    #[test]
    fn rejects_subdir_that_escapes_via_parent() {
        let mut t = r("./template");
        t.subdir = Some("../../../escape".into());
        let err = TemplateSource::from_ref(&t, Utf8Path::new("/base")).unwrap_err();
        assert!(matches!(err, Error::Template { .. }));
    }

    // Unix-only because Windows doesn't treat `/...` as absolute
    // (`Path::is_absolute` requires a drive letter or UNC prefix).
    // The behaviour we're guarding against — popping past the root
    // — is moot on Windows for `/`-style paths.
    #[cfg(unix)]
    #[test]
    fn normalise_clamps_root_on_absolute_path() {
        // `/..` should stay at `/`, not collapse to `.`.
        assert_eq!(normalise(Utf8Path::new("/..")).as_str(), "/");
        assert_eq!(normalise(Utf8Path::new("/a/..")).as_str(), "/");
    }

    #[cfg(unix)]
    #[test]
    fn normalise_preserves_leading_parent_in_relative() {
        assert_eq!(
            normalise(Utf8Path::new("../sibling")).as_str(),
            "../sibling"
        );
    }
}
