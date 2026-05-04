//! Global config (`~/.config/kata/config.toml`) — tool defaults +
//! the registry of project paths kata knows about.
//!
//! The registry is a *pointer* layer; the truth of what's installed
//! lives in each PJ's `.kata/applied.toml`. Removing a registry
//! entry doesn't touch the PJ's state.

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::AgentKind;
use crate::paths::{global_config_dir, global_config_path};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "project", skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Defaults {
    #[serde(default)]
    pub default_agent: AgentKind,
    #[serde(default = "default_ai_concurrency")]
    pub ai_concurrency: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pj_concurrency: Option<usize>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            default_agent: AgentKind::default(),
            ai_concurrency: default_ai_concurrency(),
            pj_concurrency: None,
        }
    }
}

fn default_ai_concurrency() -> usize {
    4
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: Utf8PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ProjectOverrides>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProjectOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_ai: Option<bool>,
}

impl GlobalConfig {
    /// Load from `<global_config_dir>/config.toml`. Returns
    /// `Default::default()` if the file is missing.
    pub fn load() -> Result<Self> {
        let path = global_config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(&path).map_err(|e| Error::io_at(path.as_std_path(), e))?;
        toml::from_str(&raw).map_err(|e| Error::Config(format!("{}: {}", path, e.message())))
    }

    /// Persist to `<global_config_dir>/config.toml`, creating the
    /// directory if needed.
    pub fn save(&self) -> Result<()> {
        let dir = global_config_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_at(dir.as_std_path(), e))?;
        let path = global_config_path()?;
        let body =
            toml::to_string_pretty(self).map_err(|e| Error::Config(format!("{}: {}", path, e)))?;
        std::fs::write(&path, body).map_err(|e| Error::io_at(path.as_std_path(), e))
    }

    /// Add a project entry.
    ///
    /// - Same name + same path → no-op (idempotent re-register).
    /// - Same path, different name → error (would corrupt the
    ///   registry — `find_project(path)` could return either).
    /// - Same name, different path → error.
    /// - Otherwise: appended.
    pub fn add_project(&mut self, entry: ProjectEntry) -> Result<()> {
        if let Some(existing) = self.projects.iter().find(|p| p.path == entry.path) {
            if existing.name == entry.name {
                return Ok(());
            }
            return Err(Error::Config(format!(
                "path `{}` is already registered as `{}`",
                entry.path, existing.name
            )));
        }
        if self.projects.iter().any(|p| p.name == entry.name) {
            return Err(Error::Config(format!(
                "project name `{}` is already registered",
                entry.name
            )));
        }
        self.projects.push(entry);
        Ok(())
    }

    pub fn remove_project(&mut self, key: &str) -> Result<()> {
        let before = self.projects.len();
        self.projects
            .retain(|p| p.name != key && p.path.as_str() != key);
        if self.projects.len() == before {
            return Err(Error::PjUnknown(key.to_string()));
        }
        Ok(())
    }

    pub fn find_project(&self, key: &str) -> Option<&ProjectEntry> {
        self.projects
            .iter()
            .find(|p| p.name == key || p.path.as_str() == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.into(),
            path: Utf8PathBuf::from(path),
            tags: vec![],
            overrides: None,
        }
    }

    #[test]
    fn add_project_rejects_duplicate_name() {
        let mut c = GlobalConfig::default();
        c.add_project(entry("a", "/p1")).unwrap();
        let err = c.add_project(entry("a", "/p2")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn add_project_is_idempotent_on_same_name_path() {
        let mut c = GlobalConfig::default();
        c.add_project(entry("a", "/p1")).unwrap();
        c.add_project(entry("a", "/p1")).unwrap();
        assert_eq!(c.projects.len(), 1);
    }

    #[test]
    fn add_project_rejects_duplicate_path_different_name() {
        let mut c = GlobalConfig::default();
        c.add_project(entry("a", "/p1")).unwrap();
        let err = c.add_project(entry("b", "/p1")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        assert_eq!(c.projects.len(), 1);
    }

    #[test]
    fn find_project_by_name_or_path() {
        let mut c = GlobalConfig::default();
        c.add_project(entry("a", "/p1")).unwrap();
        assert!(c.find_project("a").is_some());
        assert!(c.find_project("/p1").is_some());
        assert!(c.find_project("missing").is_none());
    }

    #[test]
    fn remove_project_errors_on_unknown() {
        let mut c = GlobalConfig::default();
        let err = c.remove_project("missing").unwrap_err();
        assert!(matches!(err, Error::PjUnknown(_)));
    }
}
