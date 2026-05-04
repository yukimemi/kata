//! `preset.toml` — a *bundle* of template references. Lives in an
//! external repo, gets resolved into a list of templates to apply.
//!
//! Spec grammar (settled in design):
//! `<source>[@<rev>][//<subdir>][:<preset-name>]`
//!
//! - `<source>` — `github.com/owner/repo` (git), `./local/path` or
//!   `../...` (local), or any URL git understands.
//! - `@<rev>` — branch / tag / commit. Default `main`.
//! - `//<subdir>` — Terraform-style sub-path inside the repo.
//! - `:<preset-name>` — selects which preset file when a repo
//!   carries multiple. Default `default`.
//!
//! Phase 1 implements **local sources only** (`./...` / `../...` /
//! absolute paths). Git resolution lands in Phase 2.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Preset {
    /// Display name. Should match the preset filename without
    /// extension; `kata list --preset <name>` looks it up by this.
    pub name: String,
    /// Templates in compose order (last wins on file conflicts).
    pub templates: Vec<TemplateRef>,
    /// Preset-level default vars. Manifest defaults override these;
    /// applied / preset.vars / CLI override go through the standard
    /// precedence chain elsewhere.
    #[serde(default)]
    pub vars: toml::Table,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplateRef {
    /// `github.com/...`, `./local/path`, `git+ssh://...`, etc.
    pub source: String,
    /// branch / tag / commit. Defaults to `main` for git, ignored
    /// for local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Sub-directory inside the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
}

/// Parsed components of a preset spec
/// (`<source>[@<rev>][//<subdir>][:<preset-name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetSpec {
    pub source: String,
    pub rev: Option<String>,
    pub subdir: Option<String>,
    pub preset_name: Option<String>,
}

impl PresetSpec {
    /// Parse a spec string. The grammar is forgiving: missing fields
    /// stay `None` for callers to default.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::Preset {
                path: PathBuf::from("<spec>"),
                message: "empty preset spec".into(),
            });
        }

        // Split off `:<preset-name>` from the right. Be careful not
        // to confuse it with the `:` in `git+ssh://user@host:repo`,
        // so we only honor a `:` that comes AFTER any `//` subdir
        // marker, or after the `@<rev>` marker, or in the bare
        // suffix when the source has no scheme-like `://`.
        let (rest, preset_name) = split_trailing_preset_name(s);

        // Then split off `//<subdir>` (Terraform-style — we use the
        // first `//` that isn't part of `://` scheme).
        let (rest, subdir) = split_subdir(rest);

        // Finally split off `@<rev>` from the right of what remains.
        let (source, rev) = split_rev(rest);

        if source.is_empty() {
            return Err(Error::Preset {
                path: PathBuf::from("<spec>"),
                message: format!("preset source missing in {s:?}"),
            });
        }

        Ok(Self {
            source: source.to_string(),
            rev: rev.map(str::to_string),
            subdir: subdir.map(str::to_string),
            preset_name: preset_name.map(str::to_string),
        })
    }

    /// True when `source` refers to a path on the local filesystem
    /// (Phase 1 supports only this case).
    pub fn is_local(&self) -> bool {
        let s = &self.source;
        s.starts_with("./") || s.starts_with("../") || is_absolute_local(s)
    }
}

fn is_absolute_local(s: &str) -> bool {
    // POSIX absolute, or a Windows drive-letter path (`C:\...` /
    // `C:/...`). The second case looks like a scheme but is not.
    if s.starts_with('/') {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return true;
    }
    false
}

/// Find the index of `:` that starts a trailing `:<preset-name>`,
/// or `None` if there is no such suffix. We require the suffix part
/// to contain no `/` or `\` so that we don't confuse it with a path
/// segment containing a colon.
fn split_trailing_preset_name(s: &str) -> (&str, Option<&str>) {
    if let Some(idx) = s.rfind(':') {
        let suffix = &s[idx + 1..];
        if !suffix.is_empty() && !suffix.contains('/') && !suffix.contains('\\') {
            // Reject the `:` inside `scheme://...` and Windows
            // `C:/...` paths.
            let prefix = &s[..idx];
            let is_scheme_colon = prefix.ends_with('+')
                || prefix == "git"
                || prefix == "ssh"
                || prefix == "https"
                || prefix == "http"
                || prefix == "file";
            let is_drive_colon = prefix.len() == 1
                && prefix
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic());
            if !is_scheme_colon && !is_drive_colon && !s[..idx].contains("://") {
                return (prefix, Some(suffix));
            }
            // For `scheme://`, fall through; the suffix is part of
            // the URL.
            if s[..idx].contains("://") && !suffix.contains('@') {
                return (prefix, Some(suffix));
            }
        }
    }
    (s, None)
}

fn split_subdir(s: &str) -> (&str, Option<&str>) {
    // Find the first `//` that is NOT preceded by `:` (which would
    // make it `://`).
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            if !preceded_by_colon {
                return (&s[..i], Some(&s[i + 2..]));
            }
            // Skip past the scheme `://`.
            i += 2;
            continue;
        }
        i += 1;
    }
    (s, None)
}

fn split_rev(s: &str) -> (&str, Option<&str>) {
    // `@<rev>` — but `git+ssh://user@host/...` also has `@`. Only
    // honor the LAST `@` whose suffix doesn't contain `/`, `:`, or
    // path separators.
    if let Some(idx) = s.rfind('@') {
        let suffix = &s[idx + 1..];
        if !suffix.is_empty() && !suffix.contains('/') && !suffix.contains('\\') {
            return (&s[..idx], Some(suffix));
        }
    }
    (s, None)
}

impl Preset {
    /// Load a preset from a local TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
        toml::from_str(&raw).map_err(|e| Error::preset(path, e.message()))
    }

    /// Resolve a `PresetSpec` whose source is local. The result is
    /// the parsed `Preset`. The preset filename is derived from
    /// `spec.preset_name` (defaulting to `default`) — we look for
    /// `<source>/<subdir?>/<preset-name>.toml`.
    pub fn resolve_local(spec: &PresetSpec) -> Result<Self> {
        if !spec.is_local() {
            return Err(Error::Preset {
                path: PathBuf::from(&spec.source),
                message: "git sources are not yet supported (Phase 2)".into(),
            });
        }
        let mut path = PathBuf::from(&spec.source);
        if let Some(sub) = &spec.subdir {
            path = path.join(sub);
        }
        let preset_name = spec.preset_name.as_deref().unwrap_or("default");

        // Allow either `<name>.toml` or pointing the source DIRECTLY
        // at a `.toml` file (handy for fixtures and one-off layouts).
        let candidate = if path.is_file() {
            path
        } else {
            path.join(format!("{preset_name}.toml"))
        };
        Self::load(&candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parses_simple_local_spec() {
        let s = PresetSpec::parse("./local/preset.toml").unwrap();
        assert_eq!(s.source, "./local/preset.toml");
        assert!(s.is_local());
        assert!(s.rev.is_none());
        assert!(s.subdir.is_none());
        assert!(s.preset_name.is_none());
    }

    #[test]
    fn parses_github_with_preset_name() {
        let s = PresetSpec::parse("github.com/yukimemi/pj-presets:rust-cli").unwrap();
        assert_eq!(s.source, "github.com/yukimemi/pj-presets");
        assert_eq!(s.preset_name.as_deref(), Some("rust-cli"));
        assert!(!s.is_local());
    }

    #[test]
    fn parses_full_spec() {
        let s = PresetSpec::parse("github.com/x/y@v1.0//path/to:name").unwrap();
        assert_eq!(s.source, "github.com/x/y");
        assert_eq!(s.rev.as_deref(), Some("v1.0"));
        assert_eq!(s.subdir.as_deref(), Some("path/to"));
        assert_eq!(s.preset_name.as_deref(), Some("name"));
    }

    #[test]
    fn windows_drive_letter_is_local() {
        let s = PresetSpec::parse(r"C:\Users\me\preset.toml").unwrap();
        assert!(s.is_local());
        assert_eq!(s.source, r"C:\Users\me\preset.toml");
    }

    #[test]
    fn unix_absolute_is_local() {
        let s = PresetSpec::parse("/tmp/preset.toml").unwrap();
        assert!(s.is_local());
    }

    #[test]
    fn rejects_empty() {
        assert!(PresetSpec::parse("").is_err());
        assert!(PresetSpec::parse("   ").is_err());
    }

    #[test]
    fn loads_local_preset_file() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("rust-cli.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"
            name = "rust-cli"

            [[templates]]
            source = "./pj-base"

            [[templates]]
            source = "./pj-rust"

            [vars]
            license = "MIT"
        "#
        )
        .unwrap();

        let s = PresetSpec::parse(path.to_str().unwrap()).unwrap();
        let p = Preset::resolve_local(&s).unwrap();
        assert_eq!(p.name, "rust-cli");
        assert_eq!(p.templates.len(), 2);
        assert_eq!(p.templates[0].source, "./pj-base");
    }
}
