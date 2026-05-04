//! `<pj>/.kata/applied.toml` — the **truth** of what's installed in
//! a project. Owned by kata; written automatically. Should be
//! committed by the project so teammates can `kata apply` and
//! reproduce the layout.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::{APPLIED_FILE, PJ_STATE_DIR, applied_path};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppliedState {
    /// Optional preset spec used to seed this PJ.
    /// Format: `<source>[@<rev>][//<subdir>][:<preset-name>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,

    /// Absolute directory used as the resolution base for any
    /// **relative** template `source` paths recorded below. Set by
    /// `kata init` to the directory of the preset file (so e.g.
    /// `source = "../pj-base"` resolves correctly when `kata apply`
    /// re-runs from a totally different cwd). When absent, callers
    /// fall back to the current working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<Utf8PathBuf>,

    /// Templates applied in compose order (last wins on file
    /// conflicts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<AppliedTemplate>,

    /// When the last `kata apply` finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<jiff::Timestamp>,

    /// Variable values used at apply time. Recorded so subsequent
    /// runs can re-render without re-prompting.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub vars: toml::Table,

    /// Per-file state (AI history, drift detection, once-applied
    /// markers).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, FileState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppliedTemplate {
    /// Where the template came from (URL or local path).
    pub source: String,
    /// Resolved revision (commit SHA for git, "local" for filesystem).
    pub rev: String,
    /// Sub-directory inside the source (preserves the `//<subdir>`
    /// portion of the spec so re-apply loads from the same place).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// Manifest's `version` field at apply time, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FileState {
    /// Last time the file was sent through an AI backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ai_run: Option<jiff::Timestamp>,
    /// Last user decision in the AI interactive prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_decision: Option<Decision>,
    /// SHA-256 of the file contents at last successful apply.
    /// Used for drift detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Marker for `when = "once"`: once true, kata will skip this
    /// file on future applies.
    #[serde(default, skip_serializing_if = "is_false")]
    pub once_applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Accept,
    Edit,
    Skip,
    Defer,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `base_dir` is the resolution base for *relative* template
/// sources (`./pj-base`, `..\pj-rust`). A PJ whose templates are
/// all remote URLs or absolute paths needs no base — and recording
/// an absolute one would only hurt portability when the file is
/// committed.
fn templates_need_base_dir(templates: &[AppliedTemplate]) -> bool {
    templates.iter().any(|t| is_relative_source(&t.source))
}

fn is_relative_source(s: &str) -> bool {
    s.starts_with("./") || s.starts_with("../") || s.starts_with(".\\") || s.starts_with("..\\")
}

impl AppliedState {
    /// Read state from `<pj_root>/.kata/applied.toml`. Returns
    /// `Default::default()` if the file does not exist (treating that
    /// as "never applied").
    pub fn load(pj_root: &Utf8Path) -> Result<Self> {
        let path = applied_path(pj_root);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(&path).map_err(|e| Error::io_at(path.as_std_path(), e))?;
        toml::from_str(&raw).map_err(|e| Error::applied(path.as_std_path(), e.message()))
    }

    /// Write state to `<pj_root>/.kata/applied.toml`, creating the
    /// `.kata/` directory if needed.
    ///
    /// `base_dir` is dropped from the serialised form when none of
    /// the recorded templates need it (i.e. every `source` is a
    /// remote URL or an absolute local path). This keeps committed
    /// `applied.toml` files portable across machines — only PJs that
    /// actually use `./<rel>` template sources record an absolute
    /// resolution base.
    pub fn save(&self, pj_root: &Utf8Path) -> Result<()> {
        let dir = pj_root.join(PJ_STATE_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_at(dir.as_std_path(), e))?;
        let path = dir.join(APPLIED_FILE);

        let mut view = self.clone();
        if !templates_need_base_dir(&view.templates) {
            view.base_dir = None;
        }

        let body = toml::to_string_pretty(&view)
            .map_err(|e| Error::applied(path.as_std_path(), e.to_string()))?;
        std::fs::write(&path, body).map_err(|e| Error::io_at(path.as_std_path(), e))
    }

    /// Record (or update) per-file state by destination path.
    pub fn record(&mut self, dst: impl Into<String>, state: FileState) {
        self.files.insert(dst.into(), state);
    }

    /// Append or replace a template entry. Replace happens when an
    /// entry with the same `source` already exists.
    pub fn promote_template(&mut self, t: AppliedTemplate) {
        if let Some(slot) = self.templates.iter_mut().find(|x| x.source == t.source) {
            *slot = t;
        } else {
            self.templates.push(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    #[test]
    fn round_trip_minimal() {
        let td = TempDir::new().unwrap();
        let pj = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();

        let mut s = AppliedState::default();
        s.promote_template(AppliedTemplate {
            source: "/local/pj-base".into(),
            rev: "local".into(),
            subdir: None,
            version: None,
        });
        s.record(
            "Makefile.toml",
            FileState {
                content_hash: Some("abc".into()),
                ..Default::default()
            },
        );
        s.save(&pj).unwrap();

        let loaded = AppliedState::load(&pj).unwrap();
        assert_eq!(loaded.templates.len(), 1);
        assert_eq!(loaded.templates[0].source, "/local/pj-base");
        assert_eq!(
            loaded.files["Makefile.toml"].content_hash.as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn load_returns_default_when_missing() {
        let td = TempDir::new().unwrap();
        let pj = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();
        let s = AppliedState::load(&pj).unwrap();
        assert!(s.templates.is_empty());
        assert!(s.preset.is_none());
        assert!(s.base_dir.is_none());
    }

    #[test]
    fn base_dir_is_kept_when_a_template_uses_a_relative_source() {
        let td = TempDir::new().unwrap();
        let pj = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();
        let recorded_base = Utf8PathBuf::from("/abs/preset-dir");

        let mut s = AppliedState {
            base_dir: Some(recorded_base.clone()),
            ..Default::default()
        };
        s.promote_template(AppliedTemplate {
            source: "./pj-base".into(),
            rev: "local".into(),
            subdir: None,
            version: None,
        });
        s.save(&pj).unwrap();

        let loaded = AppliedState::load(&pj).unwrap();
        assert_eq!(loaded.base_dir.as_ref(), Some(&recorded_base));
    }

    #[test]
    fn base_dir_is_dropped_when_all_sources_are_remote() {
        let td = TempDir::new().unwrap();
        let pj = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();

        let mut s = AppliedState {
            base_dir: Some(Utf8PathBuf::from("/abs/cache/slot")),
            ..Default::default()
        };
        s.promote_template(AppliedTemplate {
            source: "github.com/yukimemi/pj-base".into(),
            rev: "deadbeef".into(),
            subdir: None,
            version: None,
        });
        s.save(&pj).unwrap();

        let loaded = AppliedState::load(&pj).unwrap();
        assert!(
            loaded.base_dir.is_none(),
            "expected base_dir to be omitted from committed applied.toml \
             when no template needs a relative-source resolution base, got {:?}",
            loaded.base_dir
        );
    }

    #[test]
    fn promote_template_replaces_existing() {
        let mut s = AppliedState::default();
        s.promote_template(AppliedTemplate {
            source: "x".into(),
            rev: "1".into(),
            subdir: None,
            version: None,
        });
        s.promote_template(AppliedTemplate {
            source: "x".into(),
            rev: "2".into(),
            subdir: None,
            version: None,
        });
        s.promote_template(AppliedTemplate {
            source: "y".into(),
            rev: "1".into(),
            subdir: None,
            version: None,
        });
        assert_eq!(s.templates.len(), 2);
        assert_eq!(s.templates[0].rev, "2");
    }
}
