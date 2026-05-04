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
            let mut root = if Utf8Path::new(s).is_absolute() {
                Utf8PathBuf::from(s)
            } else {
                base_dir.join(s)
            };
            if let Some(sub) = &t.subdir {
                root = root.join(sub);
            }
            // Normalise `..` segments without requiring the path
            // to exist (canonicalize would).
            let normalised = normalise(&root);
            Ok(Self::Local { root: normalised })
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
fn normalise(p: &Utf8Path) -> Utf8PathBuf {
    let mut stack: Vec<&str> = Vec::new();
    for comp in p.components() {
        let s = comp.as_str();
        match s {
            "." => {}
            ".." => {
                if matches!(stack.last(), Some(&seg) if seg != "..") {
                    stack.pop();
                } else {
                    stack.push("..");
                }
            }
            _ => stack.push(s),
        }
    }
    if stack.is_empty() {
        return Utf8PathBuf::from(".");
    }
    let mut out = Utf8PathBuf::new();
    for seg in stack {
        out.push(seg);
    }
    out
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
}
