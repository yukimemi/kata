//! Var resolution with the precedence chain settled in design:
//!
//!   CLI (`--var name=val`) > env (`KATA_VAR_<name>`) >
//!   `.kata/vars.toml` > applied.toml > preset.vars >
//!   manifest.default > prompt
//!
//! `prompt` only fires when `interactive == true`; otherwise a
//! missing-required-without-default var is an error.

use std::collections::BTreeMap;

use camino::Utf8Path;

use crate::error::{Error, Result};
use crate::manifest::VarSpec;

const ENV_PREFIX: &str = "KATA_VAR_";

/// Per-PJ vars file path, relative to the PJ root.
const VARS_FILE_REL: &str = ".kata/vars.toml";

/// Collect inputs from each precedence layer. Lower indices in this
/// struct correspond to higher precedence.
#[derive(Debug, Clone, Default)]
pub struct VarSources {
    /// `--var name=val` from the command line.
    pub cli: BTreeMap<String, toml::Value>,
    /// `KATA_VAR_<name>` env vars (collected at runtime). Always
    /// strings.
    pub env: BTreeMap<String, toml::Value>,
    /// Values from the PJ-owned `.kata/vars.toml`. Consumer-managed
    /// (kata never writes here) — Renovate or hand edits flow into
    /// the next `kata apply` without revert.
    pub vars_file: toml::Table,
    /// Values previously recorded by kata in `applied.toml`.
    pub applied: toml::Table,
    /// Values supplied by a preset file.
    pub preset: toml::Table,
}

impl VarSources {
    /// Read every `KATA_VAR_<name>` env var into a fresh table.
    /// The name suffix is preserved verbatim — Tera (and our manifest
    /// `[vars]` table) is case-sensitive, so lowercasing here would
    /// silently break templates that declare e.g. `MyVar`.
    pub fn from_env() -> BTreeMap<String, toml::Value> {
        let mut out = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            let Ok(k) = k.into_string() else { continue };
            let Ok(v) = v.into_string() else { continue };
            if let Some(name) = k.strip_prefix(ENV_PREFIX) {
                out.insert(name.to_string(), toml::Value::String(v));
            }
        }
        out
    }

    /// Read `<pj_root>/.kata/vars.toml` into a fresh table. Missing
    /// file is not an error — vars.toml is opt-in. A present-but-
    /// malformed file is a hard error so the consumer notices their
    /// typo before it silently degrades to defaults. Read directly
    /// (rather than `exists()` then read) so a file that disappears
    /// between the check and the read isn't promoted to a hard error,
    /// and so I/O failures other than NotFound surface with the path
    /// attached via `Error::io_at`.
    pub fn load_vars_file(pj_root: &Utf8Path) -> Result<toml::Table> {
        let path = pj_root.join(VARS_FILE_REL);
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(toml::Table::new());
            }
            Err(e) => return Err(Error::io_at(path.as_std_path(), e)),
        };
        toml::from_str(&content).map_err(|e| Error::Config(format!("parse {path}: {e}")))
    }
}

/// Resolves vars by combining sources + manifest specs, optionally
/// prompting for missing values via a user-provided closure.
pub struct VarResolver<'a, F> {
    pub specs: &'a BTreeMap<String, VarSpec>,
    pub sources: &'a VarSources,
    pub interactive: bool,
    /// Called when the resolver needs to ask the user. Returns the
    /// user's answer as a `toml::Value`. Implementations live outside
    /// this module (see `interactive::prompt_var`).
    pub prompter: F,
}

impl<'a, F> VarResolver<'a, F>
where
    F: FnMut(&str, &VarSpec) -> Result<toml::Value>,
{
    pub fn resolve(mut self) -> Result<toml::Table> {
        let mut out = toml::Table::new();

        // 1) Start from the union of declared spec keys and any keys
        //    that appear in source layers (so callers can pass through
        //    extra vars not declared in the manifest).
        let mut keys: BTreeMap<String, ()> = BTreeMap::new();
        for k in self.specs.keys() {
            keys.insert(k.clone(), ());
        }
        for k in self.sources.cli.keys() {
            keys.insert(k.clone(), ());
        }
        for k in self.sources.env.keys() {
            keys.insert(k.clone(), ());
        }
        for k in self.sources.vars_file.keys() {
            keys.insert(k.clone(), ());
        }
        for k in self.sources.applied.keys() {
            keys.insert(k.clone(), ());
        }
        for k in self.sources.preset.keys() {
            keys.insert(k.clone(), ());
        }

        for (key, _) in keys {
            let spec = self.specs.get(&key);
            let value = self.resolve_one(&key, spec)?;
            if let Some(v) = value {
                out.insert(key, v);
            }
        }

        Ok(out)
    }

    fn resolve_one(&mut self, key: &str, spec: Option<&VarSpec>) -> Result<Option<toml::Value>> {
        // 1) CLI
        if let Some(v) = self.sources.cli.get(key) {
            return Ok(Some(v.clone()));
        }
        // 2) env
        if let Some(v) = self.sources.env.get(key) {
            return Ok(Some(v.clone()));
        }
        // 3) .kata/vars.toml
        if let Some(v) = self.sources.vars_file.get(key) {
            return Ok(Some(v.clone()));
        }
        // 4) applied
        if let Some(v) = self.sources.applied.get(key) {
            return Ok(Some(v.clone()));
        }
        // 5) preset
        if let Some(v) = self.sources.preset.get(key) {
            return Ok(Some(v.clone()));
        }
        // 6) manifest default
        let spec = match spec {
            Some(s) => s,
            None => return Ok(None),
        };
        if let Some(v) = &spec.default {
            return Ok(Some(v.clone()));
        }
        // 7) prompt (or error if non-interactive)
        if self.interactive {
            let v = (self.prompter)(key, spec)?;
            return Ok(Some(v));
        }
        if spec.required {
            return Err(Error::Config(format!(
                "var `{key}` is required but not provided (cli/env/.kata/vars.toml/applied/preset/default all empty)"
            )));
        }
        Ok(None)
    }
}

/// Parse a `name=value` CLI argument into a typed `toml::Value`.
/// Numbers and booleans are detected; everything else is a string.
pub fn parse_cli_var(s: &str) -> Result<(String, toml::Value)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| Error::Config(format!("--var expects `name=value`, got {s:?}")))?;
    let k = k.trim().to_string();
    let v = v.trim();
    if k.is_empty() {
        return Err(Error::Config(format!("--var has empty name in {s:?}")));
    }
    let parsed: toml::Value = if v == "true" {
        toml::Value::Boolean(true)
    } else if v == "false" {
        toml::Value::Boolean(false)
    } else if let Ok(n) = v.parse::<i64>() {
        toml::Value::Integer(n)
    } else if let Ok(n) = v.parse::<f64>() {
        toml::Value::Float(n)
    } else {
        toml::Value::String(v.to_string())
    };
    Ok((k, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_prompt(_: &str, _: &VarSpec) -> Result<toml::Value> {
        panic!("prompt should not have been called");
    }

    #[test]
    fn cli_wins_over_env_vars_file_applied_preset_default() {
        let specs = BTreeMap::from([(
            "k".to_string(),
            VarSpec {
                prompt: None,
                default: Some(toml::Value::String("from-default".into())),
                required: false,
                choices: None,
                pattern: None,
                secret: false,
            },
        )]);
        let sources = VarSources {
            cli: BTreeMap::from([("k".to_string(), toml::Value::String("from-cli".into()))]),
            env: BTreeMap::from([("k".to_string(), toml::Value::String("from-env".into()))]),
            vars_file: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-vars-file".into()),
            )]),
            applied: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-applied".into()),
            )]),
            preset: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-preset".into()),
            )]),
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out["k"].as_str(), Some("from-cli"));
    }

    #[test]
    fn vars_file_wins_over_applied_preset_default() {
        // vars.toml is the consumer's source of truth — it should
        // beat what applied.toml recorded last run, what the preset
        // shipped, and what the manifest defaults to. Only CLI / env
        // can override it.
        let specs = BTreeMap::from([(
            "k".to_string(),
            VarSpec {
                prompt: None,
                default: Some(toml::Value::String("from-default".into())),
                required: false,
                choices: None,
                pattern: None,
                secret: false,
            },
        )]);
        let sources = VarSources {
            cli: BTreeMap::default(),
            env: BTreeMap::default(),
            vars_file: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-vars-file".into()),
            )]),
            applied: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-applied".into()),
            )]),
            preset: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-preset".into()),
            )]),
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out["k"].as_str(), Some("from-vars-file"));
    }

    #[test]
    fn load_vars_file_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        let out = VarSources::load_vars_file(root).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn load_vars_file_parses_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join(".kata")).unwrap();
        std::fs::write(
            root.join(".kata/vars.toml"),
            "key = \"value\"\n[group]\nnested = 1\n",
        )
        .unwrap();
        let out = VarSources::load_vars_file(root).unwrap();
        assert_eq!(out["key"].as_str(), Some("value"));
        assert_eq!(out["group"]["nested"].as_integer(), Some(1));
    }

    #[test]
    fn load_vars_file_errors_on_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join(".kata")).unwrap();
        std::fs::write(root.join(".kata/vars.toml"), "this is = not [valid\n").unwrap();
        let err = VarSources::load_vars_file(root).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn errors_on_required_missing_non_interactive() {
        let specs = BTreeMap::from([(
            "needed".to_string(),
            VarSpec {
                prompt: None,
                default: None,
                required: true,
                choices: None,
                pattern: None,
                secret: false,
            },
        )]);
        let sources = VarSources::default();
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let err = r.resolve().unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn manifest_default_used_when_no_source() {
        let specs = BTreeMap::from([(
            "k".to_string(),
            VarSpec {
                prompt: None,
                default: Some(toml::Value::String("d".into())),
                required: false,
                choices: None,
                pattern: None,
                secret: false,
            },
        )]);
        let sources = VarSources::default();
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out["k"].as_str(), Some("d"));
    }

    #[test]
    fn parses_cli_var_typed() {
        assert_eq!(
            parse_cli_var("name=foo").unwrap(),
            ("name".into(), toml::Value::String("foo".into()))
        );
        assert_eq!(
            parse_cli_var("count=42").unwrap(),
            ("count".into(), toml::Value::Integer(42))
        );
        assert_eq!(
            parse_cli_var("flag=true").unwrap(),
            ("flag".into(), toml::Value::Boolean(true))
        );
        assert!(parse_cli_var("nope").is_err());
        assert!(parse_cli_var("=val").is_err());
    }
}
