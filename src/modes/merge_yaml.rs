//! `how = "merge-yaml"` — keep just the listed paths inside an
//! existing YAML file in sync with the template, leaving every
//! other key in place.
//!
//! Manifest:
//! ```toml
//! [[file]]
//! src   = "config.yml"
//! how   = "merge-yaml"
//! when  = "always"
//! paths = ["server.port", "logging.level"]
//! ```
//!
//! For every dotted `paths` entry kata copies the value at that
//! path from the template-rendered body into the existing file
//! at the same path, creating intermediate mappings when needed.
//! A path missing in the incoming body is left **untouched** in
//! the existing file (same conservative no-implicit-prune rule
//! `merge-toml` follows).
//!
//! **Note**: unlike `merge-toml` (toml_edit), `serde_yaml` does
//! NOT preserve key order, comments, or whitespace. Use this
//! mode for files where the value matters and the formatting
//! doesn't (typical YAML config). For files where format / order
//! / comments DO matter, write a `merge-section` block instead.
//!
//! **Regex paths (#62)**: identical rule to `merge-toml` — a
//! `paths` entry wrapped in `//...//` is parsed as a regex against
//! the incoming document's dotted-path keys, and any matching path
//! is copied from incoming to existing. Mixes freely with literal
//! paths.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_yaml::{Mapping, Value};

use crate::error::{Error, Result};

use super::merge_path::{PathSpec, parse_path_spec, shallowest_matches};
use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

pub struct MergeYaml;

#[async_trait]
impl ApplyMode for MergeYaml {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan> {
        let new_body = compute_merged(ctx)?;
        match ctx.current_body.as_deref() {
            None => Ok(ActionPlan {
                kind: PlanKind::Create,
                diff: Some(unified_diff("", &new_body, ctx.dst_abs.as_str())),
            }),
            Some(cur) if cur == new_body => Ok(ActionPlan {
                kind: PlanKind::Unchanged,
                diff: None,
            }),
            Some(cur) => Ok(ActionPlan {
                kind: PlanKind::Update,
                diff: Some(unified_diff(cur, &new_body, ctx.dst_abs.as_str())),
            }),
        }
    }

    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool) -> Result<ActionOutcome> {
        let new_body = compute_merged(ctx)?;
        let unchanged = ctx.current_body.as_deref() == Some(new_body.as_str());

        if unchanged {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Unchanged,
                decision: None,
                diff: None,
                error: None,
            });
        }

        let diff = unified_diff(
            ctx.current_body.as_deref().unwrap_or(""),
            &new_body,
            ctx.dst_abs.as_str(),
        );

        if dry_run {
            return Ok(ActionOutcome {
                kind: OutcomeKind::Skipped,
                decision: None,
                diff: Some(diff),
                error: None,
            });
        }

        if let Some(parent) = ctx.dst_abs.parent() {
            tokio::fs::create_dir_all(parent.as_std_path())
                .await
                .map_err(|e| Error::io_at(parent.as_std_path(), e))?;
        }
        tokio::fs::write(ctx.dst_abs.as_std_path(), &new_body)
            .await
            .map_err(|e| Error::io_at(ctx.dst_abs.as_std_path(), e))?;
        Ok(ActionOutcome {
            kind: OutcomeKind::Wrote,
            decision: None,
            diff: Some(diff),
            error: None,
        })
    }
}

fn compute_merged(ctx: &ActionContext<'_>) -> Result<String> {
    let paths = require_paths(ctx)?;

    let existing = match ctx.current_body.as_deref() {
        None => return Ok(ctx.rendered_body.clone()),
        Some(s) => s,
    };

    let mut existing_val: Value = serde_yaml::from_str(existing)
        .map_err(|e| Error::Merge(format!("merge-yaml: parsing existing {}: {e}", ctx.dst_abs)))?;
    let incoming_val: Value = serde_yaml::from_str(&ctx.rendered_body).map_err(|e| {
        Error::Merge(format!(
            "merge-yaml: parsing incoming for {}: {e}",
            ctx.dst_abs
        ))
    })?;

    let mut incoming_paths: Option<Vec<String>> = None;

    for path_str in paths {
        match parse_path_spec(path_str)? {
            PathSpec::Literal(lit) => {
                copy_one_path(&mut existing_val, &incoming_val, &lit)?;
            }
            PathSpec::Regex(re) => {
                let collected = incoming_paths.get_or_insert_with(|| {
                    let mut out = Vec::new();
                    collect_dotted_paths(&incoming_val, "", &mut out);
                    out
                });
                // Drop child paths when an ancestor also matches —
                // see `shallowest_matches` doc. Avoids redundant
                // tree walks on broad regexes like `//.+//`.
                let to_copy = shallowest_matches(collected, &re);
                for p in &to_copy {
                    copy_one_path(&mut existing_val, &incoming_val, p)?;
                }
            }
        }
    }

    serde_yaml::to_string(&existing_val).map_err(|e| {
        Error::Merge(format!(
            "merge-yaml: serialising merged {}: {e}",
            ctx.dst_abs
        ))
    })
}

/// Copy the value at one literal dotted path from `incoming_val`
/// into `existing_val`. Mirrors `merge-toml::copy_one_path`.
fn copy_one_path(existing_val: &mut Value, incoming_val: &Value, path_str: &str) -> Result<()> {
    let segments: Vec<&str> = path_str.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(Error::Merge(format!(
            "merge-yaml: empty segment in path `{path_str}` (e.g. trailing dot)"
        )));
    }
    if let Some(value) = value_at_path(incoming_val, &segments).cloned() {
        set_at_path(existing_val, &segments, value);
    }
    Ok(())
}

/// Recursively enumerate every dotted path in `val` — intermediate
/// mappings AND their leaves — so a regex spec can match either a
/// super-key (`^server$`) or a child (`^server\..+$`). Mirrors
/// `merge-toml::collect_dotted_paths` for the YAML data model.
fn collect_dotted_paths(val: &Value, prefix: &str, out: &mut Vec<String>) {
    let Some(map) = val.as_mapping() else {
        return;
    };
    for (key, value) in map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        let path = if prefix.is_empty() {
            key_str.to_string()
        } else {
            format!("{prefix}.{key_str}")
        };
        out.push(path.clone());
        if value.is_mapping() {
            collect_dotted_paths(value, &path, out);
        }
    }
}

/// Walk a dotted path through nested `Mapping`s and return the
/// leaf `Value` (or `None` if any segment is missing or the parent
/// isn't a mapping). Uses `Value::get(&str)` so we don't allocate
/// a `Value::String` per lookup.
fn value_at_path<'a>(val: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = val;
    for seg in path {
        match current {
            Value::Mapping(_) => current = current.get(*seg)?,
            _ => return None,
        }
    }
    Some(current)
}

/// Set the value at a dotted path, creating **missing**
/// intermediate `Mapping`s as needed. If any intermediate slot is
/// already occupied by something *other than* a mapping the call
/// is a silent no-op — same conservative refuse-to-clobber
/// contract `merge-toml::set_at_path` enforces (see PR #12
/// for the rationale).
fn set_at_path(val: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        return;
    }

    let mut current: &mut Value = val;
    for &seg in &path[..path.len() - 1] {
        if !current.is_mapping() {
            return;
        }
        let map = current.as_mapping_mut().expect("just checked is_mapping");
        if !map.contains_key(seg) {
            map.insert(
                Value::String(seg.to_string()),
                Value::Mapping(Mapping::new()),
            );
        }
        current = map.get_mut(seg).expect("just inserted");
    }

    if !current.is_mapping() {
        return;
    }
    let map = current.as_mapping_mut().expect("just checked is_mapping");
    let last = path.last().expect("path is non-empty");
    map.insert(Value::String((*last).to_string()), value);
}

fn require_paths<'a>(ctx: &'a ActionContext<'_>) -> Result<&'a Vec<String>> {
    if ctx.spec.paths.is_empty() {
        return Err(Error::manifest(
            PathBuf::from(&ctx.template.source_spec),
            format!(
                "how=\"merge-yaml\" requires `paths = [...]` in `[[file]]` for {}",
                ctx.spec.src
            ),
        ));
    }
    Ok(&ctx.spec.paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge(existing: Option<&str>, incoming: &str, paths: &[&str]) -> String {
        match existing {
            None => incoming.to_string(),
            Some(existing) => {
                let mut existing_val: Value = serde_yaml::from_str(existing).unwrap();
                let incoming_val: Value = serde_yaml::from_str(incoming).unwrap();
                let mut incoming_paths: Option<Vec<String>> = None;
                for path_str in paths {
                    match parse_path_spec(path_str).unwrap() {
                        PathSpec::Literal(lit) => {
                            copy_one_path(&mut existing_val, &incoming_val, &lit).unwrap();
                        }
                        PathSpec::Regex(re) => {
                            let collected = incoming_paths.get_or_insert_with(|| {
                                let mut out = Vec::new();
                                collect_dotted_paths(&incoming_val, "", &mut out);
                                out
                            });
                            for p in &shallowest_matches(collected, &re) {
                                copy_one_path(&mut existing_val, &incoming_val, p).unwrap();
                            }
                        }
                    }
                }
                serde_yaml::to_string(&existing_val).unwrap()
            }
        }
    }

    #[test]
    fn merge_replaces_only_listed_path() {
        let existing = "\
server:
  host: localhost
  port: 8080
logging:
  level: info
";
        let incoming = "\
server:
  port: 9090
";
        let merged = merge(Some(existing), incoming, &["server.port"]);
        // merged is YAML — parse it back to assert
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["server"]["port"], Value::Number(9090.into()));
        assert_eq!(v["server"]["host"], Value::String("localhost".into()));
        assert_eq!(v["logging"]["level"], Value::String("info".into()));
    }

    #[test]
    fn merge_creates_intermediate_mappings() {
        let existing = "name: demo\n";
        let incoming = "\
name: demo
deps:
  serde: '1'
";
        let merged = merge(Some(existing), incoming, &["deps.serde"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["deps"]["serde"], Value::String("1".into()));
        assert_eq!(v["name"], Value::String("demo".into()));
    }

    #[test]
    fn merge_skips_path_missing_from_incoming() {
        let existing = "deps:\n  serde: '1'\n";
        let incoming = "deps:\n  clap: '4'\n"; // no serde
        let merged = merge(Some(existing), incoming, &["deps.serde"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["deps"]["serde"], Value::String("1".into()));
        // the lookup didn't include deps.clap, so existing stays
        assert!(v["deps"].get("clap").is_none() || v["deps"]["clap"] == Value::Null);
    }

    #[test]
    fn merge_does_not_touch_unlisted_paths() {
        let existing = "\
a:
  keep: 1
b:
  also_keep: 2
";
        let incoming = "\
a:
  keep: 99
b:
  also_keep: 88
";
        let merged = merge(Some(existing), incoming, &["a.keep"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["a"]["keep"], Value::Number(99.into()));
        assert_eq!(v["b"]["also_keep"], Value::Number(2.into()));
    }

    #[test]
    fn merge_creates_full_file_when_dst_absent() {
        let incoming = "name: x\n";
        let merged = merge(None, incoming, &["name"]);
        assert_eq!(merged, incoming);
    }

    #[test]
    fn merge_refuses_to_clobber_non_mapping_intermediate() {
        // Same shape as the merge-toml refuse-to-clobber test from
        // PR #12: existing has `package` as a scalar string. Path
        // `package.name` would need to walk into a non-mapping —
        // set_at_path bails out, the scalar survives.
        let existing = "package: as-a-string\n";
        let incoming = "package:\n  name: new\n";
        let merged = merge(Some(existing), incoming, &["package.name"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        // `package` is still the original scalar
        assert_eq!(v["package"], Value::String("as-a-string".into()));
    }

    #[test]
    fn regex_path_sweeps_all_server_subkeys() {
        // Issue #62 — same rvpm-style `//pattern//` form merge-toml
        // accepts. A single regex replaces every named entry under
        // `server.*` without enumerating each one.
        let existing = "\
server:
  host: localhost
  port: 8080
logging:
  level: info
";
        let incoming = "\
server:
  host: prod.example.com
  port: 443
  tls: true
logging:
  level: debug
";
        let merged = merge(Some(existing), incoming, &[r"//^server\..+$//"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(
            v["server"]["host"],
            Value::String("prod.example.com".into())
        );
        assert_eq!(v["server"]["port"], Value::Number(443.into()));
        assert_eq!(v["server"]["tls"], Value::Bool(true));
        // Logging path was NOT in the regex — should keep `info`.
        assert_eq!(v["logging"]["level"], Value::String("info".into()));
    }

    #[test]
    fn regex_and_literal_paths_compose() {
        // Same composition test as merge-toml: one regex + one
        // literal in the same list both fire.
        let existing = "\
a:
  keep_a: 1
b:
  keep_b: 2
";
        let incoming = "\
a:
  keep_a: 99
b:
  keep_b: 88
  nested: new
";
        let merged = merge(Some(existing), incoming, &["a.keep_a", r"//^b\..+$//"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["a"]["keep_a"], Value::Number(99.into()));
        assert_eq!(v["b"]["keep_b"], Value::Number(88.into()));
        assert_eq!(v["b"]["nested"], Value::String("new".into()));
    }
}
