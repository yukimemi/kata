//! Var resolution with provenance.
//!
//! Precedence chain (lower-indexed wins):
//!
//!   CLI (`--var name=val`) > env (`KATA_VAR_<name>`) >
//!   `.kata/vars.toml` > applied.toml > preset.vars >
//!   template-side `vars.toml` seed > manifest.default > prompt
//!
//! `prompt` only fires when `interactive == true`; otherwise a
//! missing-required-without-default var is an error.
//!
//! Resolution returns both the resolved values AND a per-key
//! provenance map (`VarSource`) so callers can act differently on
//! "the user typed this" (Cli/Env/Prompt) vs "the template shipped
//! this" (VarsFile/Preset/TemplateSeed/Default). The runner uses
//! that to keep `applied.toml.vars` free of values that already
//! live in a tracked file — see yukimemi/kata#58.

use std::collections::BTreeMap;

use camino::Utf8Path;

use crate::error::{Error, Result};
use crate::manifest::VarSpec;

const ENV_PREFIX: &str = "KATA_VAR_";

/// Per-PJ vars file path, relative to the PJ root.
const VARS_FILE_REL: &str = ".kata/vars.toml";

/// Where a resolved var came from. Used downstream by the runner to
/// decide which vars to persist in `applied.toml.vars` (only the
/// "user-typed" sources — see yukimemi/kata#58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarSource {
    /// `--var name=val` from the command line.
    Cli,
    /// `KATA_VAR_<name>` from the environment.
    Env,
    /// `<pj_root>/.kata/vars.toml`.
    VarsFile,
    /// `applied.toml.vars` from a previous apply.
    Applied,
    /// Preset file vars.
    Preset,
    /// The `vars.toml` source file declared by a template via
    /// `[[file]] dst = ".kata/vars.toml"`. Auto-loaded so the
    /// renderer sees the seeded values on the **first** apply,
    /// before kata writes the seed to disk (yukimemi/kata#53).
    TemplateSeed,
    /// `[vars] default = …` in a template manifest.
    Default,
    /// User answered an interactive prompt during this apply.
    Prompt,
}

impl VarSource {
    /// Returns `true` when the resolved value should be re-recorded
    /// in `applied.toml.vars`. The persist set is "values whose only
    /// other home is the user's memory" — i.e. user-typed inputs
    /// (Cli / Env / Prompt) AND the carry-forward of those from a
    /// previous apply (Applied itself). Without persisting `Applied`,
    /// a one-shot `--var foo=bar` on run 1 would survive run 2 (still
    /// in applied) but vanish on run 3 (run 2 wrote nothing because
    /// the source was now `Applied`, not `Cli`).
    ///
    /// Returns `false` for sources that already live in a tracked
    /// file (VarsFile / Preset / TemplateSeed) or regenerate every
    /// apply (Default) — duplicating them in applied.toml bloats the
    /// file and conflates kata's implicit memory with consumer-owned
    /// configuration.
    pub fn should_persist_in_applied(self) -> bool {
        matches!(
            self,
            VarSource::Cli | VarSource::Env | VarSource::Prompt | VarSource::Applied
        )
    }
}

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
    /// Template-side `vars.toml` content (the source of any
    /// `[[file]]` declaration with `dst = ".kata/vars.toml"`).
    /// Auto-merged from every template in compose order — see
    /// `VarSources::load_template_seed`.
    pub template_seed: toml::Table,
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

/// Deep-merge `src` into `dst` (used to combine each template's
/// `vars.toml` seed in compose order). Tables merge key-by-key
/// recursively; non-table values get replaced wholesale (later wins).
pub fn deep_merge_table(dst: &mut toml::Table, src: toml::Table) {
    for (k, v) in src {
        match (dst.get_mut(&k), v) {
            (Some(toml::Value::Table(dst_t)), toml::Value::Table(src_t)) => {
                deep_merge_table(dst_t, src_t);
            }
            (_, v) => {
                dst.insert(k, v);
            }
        }
    }
}

/// Output of [`VarResolver::resolve`] — both the flat values table
/// (for Tera context construction) and the per-key provenance map
/// (for the runner's `applied.toml` filter).
#[derive(Debug, Clone, Default)]
pub struct ResolvedVars {
    pub values: toml::Table,
    pub sources: BTreeMap<String, VarSource>,
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
    pub fn resolve(mut self) -> Result<ResolvedVars> {
        let mut out = ResolvedVars::default();

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
        for k in self.sources.template_seed.keys() {
            keys.insert(k.clone(), ());
        }

        for (key, _) in keys {
            let spec = self.specs.get(&key);
            if let Some((value, source)) = self.resolve_one(&key, spec)? {
                out.values.insert(key.clone(), value);
                out.sources.insert(key, source);
            }
        }

        Ok(out)
    }

    fn resolve_one(
        &mut self,
        key: &str,
        spec: Option<&VarSpec>,
    ) -> Result<Option<(toml::Value, VarSource)>> {
        // 1) CLI
        if let Some(v) = self.sources.cli.get(key) {
            return Ok(Some((v.clone(), VarSource::Cli)));
        }
        // 2) env
        if let Some(v) = self.sources.env.get(key) {
            return Ok(Some((v.clone(), VarSource::Env)));
        }
        // 3) .kata/vars.toml
        if let Some(v) = self.sources.vars_file.get(key) {
            return Ok(Some((v.clone(), VarSource::VarsFile)));
        }
        // 4) applied
        if let Some(v) = self.sources.applied.get(key) {
            return Ok(Some((v.clone(), VarSource::Applied)));
        }
        // 5) preset
        if let Some(v) = self.sources.preset.get(key) {
            return Ok(Some((v.clone(), VarSource::Preset)));
        }
        // 6) template-side vars.toml seed
        if let Some(v) = self.sources.template_seed.get(key) {
            return Ok(Some((v.clone(), VarSource::TemplateSeed)));
        }
        // 7) manifest default
        let spec = match spec {
            Some(s) => s,
            None => return Ok(None),
        };
        if let Some(v) = &spec.default {
            return Ok(Some((v.clone(), VarSource::Default)));
        }
        // 8) prompt (or error if non-interactive)
        if self.interactive {
            let v = (self.prompter)(key, spec)?;
            return Ok(Some((v, VarSource::Prompt)));
        }
        if spec.required {
            return Err(Error::Config(format!(
                "var `{key}` is required but not provided (cli/env/.kata/vars.toml/applied/preset/template-seed/default all empty)"
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

    fn spec_with_default(default: &str) -> BTreeMap<String, VarSpec> {
        BTreeMap::from([(
            "k".to_string(),
            VarSpec {
                prompt: None,
                default: Some(toml::Value::String(default.into())),
                required: false,
                choices: None,
                pattern: None,
                secret: false,
            },
        )])
    }

    #[test]
    fn cli_wins_over_every_other_source() {
        let specs = spec_with_default("from-default");
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
            template_seed: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-template-seed".into()),
            )]),
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.values["k"].as_str(), Some("from-cli"));
        assert_eq!(out.sources["k"], VarSource::Cli);
    }

    #[test]
    fn vars_file_wins_over_applied_preset_template_seed_default() {
        let specs = spec_with_default("from-default");
        let sources = VarSources {
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
            template_seed: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-template-seed".into()),
            )]),
            ..Default::default()
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.values["k"].as_str(), Some("from-vars-file"));
        assert_eq!(out.sources["k"], VarSource::VarsFile);
    }

    #[test]
    fn template_seed_feeds_renderer_when_no_vars_file_yet() {
        // The yukimemi/kata#53 case: a fresh consumer has no
        // `.kata/vars.toml` yet, so the template-side seed is the only
        // place the renderer can find action versions on the first
        // apply. preset, applied, and CLI/env are all empty.
        let specs = BTreeMap::new();
        let sources = VarSources {
            template_seed: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-template-seed".into()),
            )]),
            ..Default::default()
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.values["k"].as_str(), Some("from-template-seed"));
        assert_eq!(out.sources["k"], VarSource::TemplateSeed);
    }

    #[test]
    fn template_seed_is_below_preset() {
        // If a preset pins a different version than the template ships,
        // the preset wins. Template seed is only the fallback for
        // first-apply rendering — once a preset / vars.toml / applied
        // / CLI / env tracks the value, those take over.
        let specs = BTreeMap::new();
        let sources = VarSources {
            preset: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-preset".into()),
            )]),
            template_seed: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-template-seed".into()),
            )]),
            ..Default::default()
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.values["k"].as_str(), Some("from-preset"));
        assert_eq!(out.sources["k"], VarSource::Preset);
    }

    #[test]
    fn template_seed_above_manifest_default() {
        // Template's seed should beat the manifest's `default = ...`,
        // because seeds carry concrete pins (often action versions)
        // that the template wants to be the actual starting state,
        // not just a fallback.
        let specs = spec_with_default("from-default");
        let sources = VarSources {
            template_seed: toml::Table::from_iter([(
                "k".to_string(),
                toml::Value::String("from-template-seed".into()),
            )]),
            ..Default::default()
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.values["k"].as_str(), Some("from-template-seed"));
        assert_eq!(out.sources["k"], VarSource::TemplateSeed);
    }

    #[test]
    fn provenance_tracks_each_source_correctly() {
        // Multiple keys, each resolved from a different source — make
        // sure the per-key provenance map distinguishes them. Drives
        // the applied-toml-vars filter in the runner.
        let specs = BTreeMap::from([(
            "from_default_key".to_string(),
            VarSpec {
                prompt: None,
                default: Some(toml::Value::String("d".into())),
                required: false,
                choices: None,
                pattern: None,
                secret: false,
            },
        )]);
        let sources = VarSources {
            cli: BTreeMap::from([("cli_key".to_string(), toml::Value::String("c".into()))]),
            env: BTreeMap::from([("env_key".to_string(), toml::Value::String("e".into()))]),
            vars_file: toml::Table::from_iter([(
                "vars_file_key".to_string(),
                toml::Value::String("vf".into()),
            )]),
            applied: toml::Table::from_iter([(
                "applied_key".to_string(),
                toml::Value::String("a".into()),
            )]),
            preset: toml::Table::from_iter([(
                "preset_key".to_string(),
                toml::Value::String("p".into()),
            )]),
            template_seed: toml::Table::from_iter([(
                "template_seed_key".to_string(),
                toml::Value::String("ts".into()),
            )]),
        };
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.sources["cli_key"], VarSource::Cli);
        assert_eq!(out.sources["env_key"], VarSource::Env);
        assert_eq!(out.sources["vars_file_key"], VarSource::VarsFile);
        assert_eq!(out.sources["applied_key"], VarSource::Applied);
        assert_eq!(out.sources["preset_key"], VarSource::Preset);
        assert_eq!(out.sources["template_seed_key"], VarSource::TemplateSeed);
        assert_eq!(out.sources["from_default_key"], VarSource::Default);
    }

    #[test]
    fn should_persist_in_applied_includes_user_typed_and_applied_carry() {
        // The whole point of provenance: applied.toml.vars stays free
        // of values that already live in a tracked source (yukimemi/kata#58).
        // But `Applied` itself MUST persist — it's the carry-forward
        // of a previous CLI/Env/Prompt answer, and dropping it would
        // make those answers survive only one apply.
        assert!(VarSource::Cli.should_persist_in_applied());
        assert!(VarSource::Env.should_persist_in_applied());
        assert!(VarSource::Prompt.should_persist_in_applied());
        assert!(VarSource::Applied.should_persist_in_applied());
        assert!(!VarSource::VarsFile.should_persist_in_applied());
        assert!(!VarSource::Preset.should_persist_in_applied());
        assert!(!VarSource::TemplateSeed.should_persist_in_applied());
        assert!(!VarSource::Default.should_persist_in_applied());
    }

    #[test]
    fn deep_merge_table_combines_nested_keys() {
        let mut dst = toml::Table::new();
        dst.insert(
            "actions".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "checkout".to_string(),
                toml::Value::String("v6".into()),
            )])),
        );
        let src = toml::Table::from_iter([(
            "actions".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "swatinem_rust_cache".to_string(),
                toml::Value::String("v2".into()),
            )])),
        )]);
        deep_merge_table(&mut dst, src);
        let actions = dst["actions"].as_table().unwrap();
        assert_eq!(actions["checkout"].as_str(), Some("v6"));
        assert_eq!(actions["swatinem_rust_cache"].as_str(), Some("v2"));
    }

    #[test]
    fn deep_merge_table_later_wins_on_leaf_conflict() {
        let mut dst =
            toml::Table::from_iter([("k".to_string(), toml::Value::String("first".into()))]);
        deep_merge_table(
            &mut dst,
            toml::Table::from_iter([("k".to_string(), toml::Value::String("second".into()))]),
        );
        assert_eq!(dst["k"].as_str(), Some("second"));
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
        let specs = spec_with_default("d");
        let sources = VarSources::default();
        let r = VarResolver {
            specs: &specs,
            sources: &sources,
            interactive: false,
            prompter: never_prompt,
        };
        let out = r.resolve().unwrap();
        assert_eq!(out.values["k"].as_str(), Some("d"));
        assert_eq!(out.sources["k"], VarSource::Default);
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
