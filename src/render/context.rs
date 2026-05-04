//! Tera context assembly. Three top-level namespaces are exposed
//! to every template:
//!
//! - `system.*` — os/arch/user/host/cwd (provided by teravars).
//! - `kata.*`   — kata version + helpful runtime metadata.
//! - `project.*` — name / path / tags from the registry entry.
//! - `vars.*`   — the resolved vars table.

use camino::Utf8Path;
use serde::Serialize;
use teravars::{Context, system_context};

use crate::config::ProjectEntry;

/// Information about kata itself, exposed under `kata.*` in templates.
#[derive(Serialize)]
pub struct KataInfo {
    pub version: &'static str,
}

impl KataInfo {
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// Per-project info exposed under `project.*`.
#[derive(Serialize)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    pub tags: Vec<String>,
}

impl ProjectInfo {
    pub fn from_entry(entry: &ProjectEntry, pj_root: &Utf8Path) -> Self {
        Self {
            name: entry.name.clone(),
            path: pj_root.as_str().to_string(),
            tags: entry.tags.clone(),
        }
    }
}

/// Build the full Tera context for rendering a template against a
/// project. `vars` is typically the output of `VarResolver::resolve()`.
pub fn build_context(project: &ProjectEntry, pj_root: &Utf8Path, vars: &toml::Table) -> Context {
    let mut ctx: Context = system_context();
    ctx.insert("kata", &KataInfo::current());
    ctx.insert("project", &ProjectInfo::from_entry(project, pj_root));
    ctx.insert("vars", vars);
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn entry() -> ProjectEntry {
        ProjectEntry {
            name: "demo".into(),
            path: Utf8PathBuf::from("/tmp/demo"),
            tags: vec!["rust".into()],
            overrides: None,
        }
    }

    #[test]
    fn context_carries_all_namespaces() {
        use teravars::Engine;
        let mut vars = toml::Table::new();
        vars.insert("greeting".into(), toml::Value::String("hello".into()));
        let ctx = build_context(&entry(), Utf8Path::new("/tmp/demo"), &vars);

        let mut e = Engine::new();
        let probe = e
            .render(
                "{{ system.os }}|{{ kata.version }}|{{ project.name }}|{{ vars.greeting }}",
                &ctx,
            )
            .unwrap();
        let parts: Vec<&str> = probe.split('|').collect();
        assert!(!parts[0].is_empty(), "system.os should be populated");
        assert!(!parts[1].is_empty(), "kata.version should be populated");
        assert_eq!(parts[2], "demo");
        assert_eq!(parts[3], "hello");
    }
}
