//! Templating engine. Wraps `teravars::Engine` so kata can register
//! kata-specific helpers in one place (none yet for Phase 1).

pub mod context;
pub mod vars;

use teravars::{Context, Engine};

use crate::error::Result;

pub use context::{KataInfo, ProjectInfo, build_context};
pub use vars::{ResolvedVars, VarResolver, VarSource, VarSources, deep_merge_table, parse_cli_var};

/// kata's renderer. The wrapper exists so we can register
/// kata-flavour helpers / filters without leaking the dependency on
/// `teravars::Engine` to every callsite.
pub struct Renderer {
    engine: Engine,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            engine: Engine::new(),
        }
    }

    pub fn render(&mut self, raw: &str, ctx: &Context) -> Result<String> {
        Ok(self.engine.render(raw, ctx)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectEntry;
    use camino::{Utf8Path, Utf8PathBuf};

    fn entry() -> ProjectEntry {
        ProjectEntry {
            name: "demo".into(),
            path: Utf8PathBuf::from("/tmp/demo"),
            tags: vec![],
            overrides: None,
        }
    }

    #[test]
    fn renders_with_full_context() {
        let mut vars = toml::Table::new();
        vars.insert("who".into(), toml::Value::String("world".into()));
        let ctx = build_context(&entry(), Utf8Path::new("/tmp/demo"), &vars);

        let mut r = Renderer::new();
        let out = r
            .render(
                "hello {{ vars.who }} from {{ project.name }} on {{ system.os }}",
                &ctx,
            )
            .unwrap();
        assert!(out.starts_with("hello world from demo on "));
    }
}
