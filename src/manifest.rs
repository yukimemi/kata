//! `template.toml` schema — owned by template authors. Parsed by kata
//! when applying a template to a project.
//!
//! ```toml
//! name = "pj-rust-cli"
//! version = "0.1.0"
//!
//! [vars]
//! project = { prompt = "project name?", required = true }
//! license = { choices = ["MIT", "Apache-2.0"], default = "MIT" }
//!
//! [[file]]
//! src = "Makefile.toml"
//! how = "overwrite"
//! when = "always"
//!
//! [[file]]
//! src = "src/main.rs"
//! how = "overwrite"
//! when = "once"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Filename kata looks for inside a template repo.
pub const MANIFEST_FILE: &str = "template.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Identifier shown in apply logs. Should match the directory /
    /// repo name by convention.
    pub name: String,
    /// Optional SemVer; reserved for breaking-change detection.
    #[serde(default)]
    pub version: Option<String>,
    /// File-level rules. Renamed from `file` in the TOML for
    /// readability (`[[file]]`).
    #[serde(default, rename = "file")]
    pub files: Vec<FileSpec>,
    /// Variable declarations.
    #[serde(default)]
    pub vars: BTreeMap<String, VarSpec>,
    /// Optional preconditions (PJ must satisfy before this template
    /// can apply).
    #[serde(default)]
    pub requires: Requires,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileSpec {
    /// Path inside the template directory. Globs allowed in later
    /// phases; for Phase 1 treat as a literal relative path.
    pub src: String,
    /// PJ-side relative path. Defaults to `src` if omitted.
    /// Tera-templated.
    #[serde(default)]
    pub dst: Option<String>,
    /// How to apply this file.
    pub how: HowMode,
    /// When to apply. Defaults to `always`.
    #[serde(default)]
    pub when: WhenMode,
    /// Optional Tera bool predicate; false = skip.
    #[serde(default)]
    pub when_expr: Option<String>,
    /// AI agent override (for `how = "ai"`).
    #[serde(default)]
    pub agent: Option<AgentKind>,
    /// AI prompt (Tera-templated). Required for `how = "ai"`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Marker pair for `merge-section`.
    #[serde(default)]
    pub marker: Option<MarkerSpec>,
    /// Path expressions for `merge-toml` / `merge-yaml`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Script spec for `how = "script"`.
    #[serde(default)]
    pub run: Option<ScriptSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HowMode {
    Overwrite,
    MergeSection,
    MergeToml,
    MergeYaml,
    Ai,
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenMode {
    Once,
    #[default]
    Always,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Auto,
    Claude,
    Gemini,
    Codex,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VarSpec {
    /// Prompt text shown when interactively asking for the value.
    /// Defaults to the variable name.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Default value (used when prompt is skipped).
    #[serde(default)]
    pub default: Option<toml::Value>,
    /// If true, the value must be provided (no silent default).
    #[serde(default)]
    pub required: bool,
    /// Restricted choices; presents a Select prompt.
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    /// Optional regex for validation.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Echo-suppressed input (Password prompt).
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarkerSpec {
    pub begin: String,
    pub end: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScriptSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Requires {
    /// Files that must already exist in the PJ before this template
    /// applies (e.g. `["Cargo.toml"]` for a Rust template).
    #[serde(default)]
    pub files: Vec<String>,
    /// Allowed `system.os` values; empty = no restriction.
    #[serde(default)]
    pub os: Vec<String>,
}

impl Manifest {
    /// Load and parse a manifest from a file path.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
        Self::from_str(&raw, path)
    }

    /// Parse manifest TOML from a string. `path` is used only for
    /// error reporting.
    pub fn from_str(raw: &str, path: &Path) -> Result<Self> {
        toml::from_str::<Self>(raw).map_err(|e| Error::manifest(path, e.message()))
    }
}

impl FileSpec {
    /// Resolved destination path: `dst` if given, otherwise `src`.
    pub fn dst_or_src(&self) -> &str {
        self.dst.as_deref().unwrap_or(&self.src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_minimal_manifest() {
        let raw = r#"
            name = "demo"
            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
        "#;
        let m = Manifest::from_str(raw, &PathBuf::from("test.toml")).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].how, HowMode::Overwrite);
        assert_eq!(m.files[0].when, WhenMode::Always);
        assert_eq!(m.files[0].dst_or_src(), "Makefile.toml");
    }

    #[test]
    fn parses_full_manifest() {
        let raw = r#"
            name = "rust-cli"
            version = "0.1.0"

            [vars]
            project = { prompt = "name?", required = true }
            license = { choices = ["MIT", "Apache-2.0"], default = "MIT" }

            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            when = "always"

            [[file]]
            src = "src/main.rs"
            how = "overwrite"
            when = "once"

            [[file]]
            src = "CLAUDE.md"
            how = "ai"
            agent = "claude"
            prompt = "merge"
        "#;
        let m = Manifest::from_str(raw, &PathBuf::from("test.toml")).unwrap();
        assert_eq!(m.name, "rust-cli");
        assert_eq!(m.version.as_deref(), Some("0.1.0"));
        assert_eq!(m.vars.len(), 2);
        assert!(m.vars["project"].required);
        assert_eq!(
            m.vars["license"].choices.as_ref().unwrap(),
            &vec!["MIT".to_string(), "Apache-2.0".to_string()]
        );
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.files[1].when, WhenMode::Once);
        assert_eq!(m.files[2].how, HowMode::Ai);
        assert_eq!(m.files[2].agent, Some(AgentKind::Claude));
    }

    #[test]
    fn rejects_unknown_how() {
        let raw = r#"
            name = "x"
            [[file]]
            src = "f"
            how = "wat"
        "#;
        let err = Manifest::from_str(raw, &PathBuf::from("t.toml")).unwrap_err();
        assert!(matches!(err, Error::Manifest { .. }));
    }
}
