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

use std::path::PathBuf;

use async_trait::async_trait;
use serde_yaml::{Mapping, Value};

use crate::error::{Error, Result};

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

    for path_str in paths {
        let segments: Vec<&str> = path_str.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(Error::Merge(format!(
                "merge-yaml: empty segment in path `{path_str}` (e.g. trailing dot)"
            )));
        }
        if let Some(value) = value_at_path(&incoming_val, &segments).cloned() {
            set_at_path(&mut existing_val, &segments, value);
        }
        // path absent in incoming → leave existing untouched
    }

    serde_yaml::to_string(&existing_val).map_err(|e| {
        Error::Merge(format!(
            "merge-yaml: serialising merged {}: {e}",
            ctx.dst_abs
        ))
    })
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
                for path_str in paths {
                    let segments: Vec<&str> = path_str.split('.').collect();
                    if let Some(v) = value_at_path(&incoming_val, &segments).cloned() {
                        set_at_path(&mut existing_val, &segments, v);
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
}
