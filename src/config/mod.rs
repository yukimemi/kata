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

/// How kata reacts to a newer GitHub release when a background
/// update check runs at the start of each command.
///
/// - `off` — no background check, no install (set `KATA_NO_AUTOUPDATE`
///   in the environment for a one-shot, config-independent opt-out).
/// - `notify` — check only; print a banner pointing at
///   `kata self-update` when a newer release exists. Never installs.
/// - `install` (default) — silently download and swap the binary in
///   the background. The running process keeps the old binary; the
///   new version applies on the next launch.
///
/// All network / lock failures are swallowed silently (resilience),
/// and development builds are never updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AutoUpdateMode {
    /// No background check, no install.
    Off,
    /// Check only; print a banner, never install.
    Notify,
    /// Silently self-install in the background (default).
    #[default]
    Install,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Defaults {
    #[serde(default)]
    pub default_agent: AgentKind,
    #[serde(default = "default_ai_concurrency")]
    pub ai_concurrency: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pj_concurrency: Option<usize>,
    /// Background auto-update behaviour. Defaults to `install`
    /// (opt-out silent self-update). The `KATA_NO_AUTOUPDATE` env
    /// var overrides this to `off` for a single invocation.
    #[serde(default)]
    pub auto_update: AutoUpdateMode,
    /// Minimum interval between background update checks (humantime
    /// format: `"24h"`, `"6h"`, `"1d"`). Unset → 24h. Invalid values
    /// fall back to 24h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_check_interval: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            default_agent: AgentKind::default(),
            ai_concurrency: default_ai_concurrency(),
            pj_concurrency: None,
            auto_update: AutoUpdateMode::default(),
            update_check_interval: None,
        }
    }
}

impl Defaults {
    /// Resolve the configured auto-update mode. A single chokepoint so
    /// any future config-level folding logic lives in one place. The
    /// `KATA_NO_AUTOUPDATE` env kill-switch override is applied
    /// separately in `updater::resolve_mode`, not here.
    #[must_use]
    pub fn update_mode(&self) -> AutoUpdateMode {
        self.auto_update
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

    // ---- auto_update --------------------------------------------------

    #[test]
    fn auto_update_defaults_to_install_when_defaults_absent() {
        // No `[defaults]` section at all → whole config defaults.
        let c: GlobalConfig = toml::from_str("").unwrap();
        assert_eq!(c.defaults.auto_update, AutoUpdateMode::Install);
        // Construct-vs-parse parity: the hand-written impl Default must agree.
        assert_eq!(
            GlobalConfig::default().defaults.auto_update,
            AutoUpdateMode::Install
        );
    }

    #[test]
    fn auto_update_defaults_to_install_when_present_but_unset() {
        // `[defaults]` present but `auto_update` omitted.
        let c: GlobalConfig = toml::from_str("[defaults]\nai_concurrency = 8\n").unwrap();
        assert_eq!(c.defaults.auto_update, AutoUpdateMode::Install);
    }

    #[test]
    fn auto_update_parses_each_variant() {
        for (raw, want) in [
            ("off", AutoUpdateMode::Off),
            ("notify", AutoUpdateMode::Notify),
            ("install", AutoUpdateMode::Install),
        ] {
            let c: GlobalConfig =
                toml::from_str(&format!("[defaults]\nauto_update = \"{raw}\"\n")).unwrap();
            assert_eq!(c.defaults.auto_update, want, "parsing {raw:?}");
        }
    }

    #[test]
    fn update_mode_resolver_returns_field() {
        let mut d = Defaults::default();
        assert_eq!(d.update_mode(), AutoUpdateMode::Install);
        d.auto_update = AutoUpdateMode::Off;
        assert_eq!(d.update_mode(), AutoUpdateMode::Off);
        d.auto_update = AutoUpdateMode::Notify;
        assert_eq!(d.update_mode(), AutoUpdateMode::Notify);
    }

    #[test]
    fn auto_update_round_trips_through_save_format() {
        // Defaults derives Serialize and is written back by save(); make sure
        // a parsed mode survives a serialize → deserialize round-trip.
        let c: GlobalConfig = toml::from_str("[defaults]\nauto_update = \"notify\"\n").unwrap();
        let body = toml::to_string_pretty(&c).unwrap();
        let back: GlobalConfig = toml::from_str(&body).unwrap();
        assert_eq!(back.defaults.auto_update, AutoUpdateMode::Notify);
    }

    #[test]
    fn update_check_interval_defaults_to_none_and_parses() {
        let c: GlobalConfig = toml::from_str("[defaults]\n").unwrap();
        assert_eq!(c.defaults.update_check_interval, None);
        let c: GlobalConfig =
            toml::from_str("[defaults]\nupdate_check_interval = \"12h\"\n").unwrap();
        assert_eq!(c.defaults.update_check_interval.as_deref(), Some("12h"));
    }
}
