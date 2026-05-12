//! `how = "merge-json"` — keep just the listed paths inside an
//! existing JSON file in sync with the template, leaving every
//! other key alone.
//!
//! Manifest:
//! ```toml
//! [[file]]
//! src   = "renovate.json"
//! how   = "merge-json"
//! when  = "always"
//! paths = ["customManagers", "packageRules"]
//! ```
//!
//! For every dotted `paths` entry kata copies the value at that
//! path from the template-rendered body into the existing file
//! at the same path, creating intermediate `{}` objects when
//! needed. A path missing in the incoming body is left
//! **untouched** in the existing file (same conservative
//! no-implicit-prune rule that `merge-toml` and `merge-yaml`
//! follow).
//!
//! Output formatting: kata serialises with `serde_json`'s
//! pretty-printer (2-space indent), preserving the original
//! key insertion order thanks to the `preserve_order` feature.
//! Unlike `merge-toml` we do NOT preserve comments or whitespace
//! — strict JSON has neither, and JSONC / JSON5 inputs are out
//! of scope for the first iteration of this mode (#71).
//!
//! **Path syntax limitation**: same `.`-split caveat as
//! `merge-toml` — JSON keys with literal dots inside them aren't
//! addressable. The common case (`packageRules`,
//! `customManagers`, `tsconfig.compilerOptions.strict`) works
//! fine because `.` only appears between segments.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};

use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

pub struct MergeJson;

#[async_trait]
impl ApplyMode for MergeJson {
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

    // No existing file → emit the rendered body verbatim; nothing to
    // merge into. Mirrors `merge-toml::compute_merged`.
    let existing = match ctx.current_body.as_deref() {
        None => return Ok(ctx.rendered_body.clone()),
        Some(s) => s,
    };

    let mut existing_val: Value = serde_json::from_str(existing)
        .map_err(|e| Error::Merge(format!("merge-json: parsing existing {}: {e}", ctx.dst_abs)))?;
    let incoming_val: Value = serde_json::from_str(&ctx.rendered_body).map_err(|e| {
        Error::Merge(format!(
            "merge-json: parsing incoming for {}: {e}",
            ctx.dst_abs
        ))
    })?;

    for path_str in paths {
        let segments: Vec<&str> = path_str.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(Error::Merge(format!(
                "merge-json: empty segment in path `{path_str}` (e.g. trailing dot)"
            )));
        }
        if let Some(value) = value_at_path(&incoming_val, &segments).cloned() {
            set_at_path(&mut existing_val, &segments, value);
        }
        // path absent in incoming → leave existing untouched
    }

    // Pretty-printed with 2-space indent, matching the common
    // tooling default (renovate, biome, npm, …). Trailing newline
    // so `git diff` doesn't complain.
    let mut out = serde_json::to_string_pretty(&existing_val).map_err(|e| {
        Error::Merge(format!(
            "merge-json: serialising merged {}: {e}",
            ctx.dst_abs
        ))
    })?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Walk a dotted path through nested objects and return the leaf
/// `Value` (or `None` if any segment is missing or the parent
/// isn't an object). Array indexing (`packageRules[0]`) is out of
/// scope for the first iteration; document-level paths only.
fn value_at_path<'a>(val: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = val;
    for seg in path {
        match current {
            Value::Object(_) => current = current.get(*seg)?,
            _ => return None,
        }
    }
    Some(current)
}

/// Set the value at a dotted path, creating **missing**
/// intermediate `{}` objects as needed. If any intermediate slot
/// is already occupied by something *other than* an object the
/// call is a silent no-op — same conservative refuse-to-clobber
/// contract `merge-toml::set_at_path` enforces.
fn set_at_path(val: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        return;
    }

    let mut current: &mut Value = val;
    for &seg in &path[..path.len() - 1] {
        if !current.is_object() {
            return;
        }
        let map = current.as_object_mut().expect("just checked is_object");
        if !map.contains_key(seg) {
            map.insert(seg.to_string(), Value::Object(serde_json::Map::new()));
        }
        current = map.get_mut(seg).expect("just inserted");
    }

    if !current.is_object() {
        return;
    }
    let map = current.as_object_mut().expect("just checked is_object");
    let last = path.last().expect("path is non-empty");
    map.insert((*last).to_string(), value);
}

fn require_paths<'a>(ctx: &'a ActionContext<'_>) -> Result<&'a Vec<String>> {
    if ctx.spec.paths.is_empty() {
        return Err(Error::manifest(
            PathBuf::from(&ctx.template.source_spec),
            format!(
                "how=\"merge-json\" requires `paths = [...]` in `[[file]]` for {}",
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
                let mut existing_val: Value = serde_json::from_str(existing).unwrap();
                let incoming_val: Value = serde_json::from_str(incoming).unwrap();
                for path_str in paths {
                    let segments: Vec<&str> = path_str.split('.').collect();
                    if let Some(v) = value_at_path(&incoming_val, &segments).cloned() {
                        set_at_path(&mut existing_val, &segments, v);
                    }
                }
                serde_json::to_string_pretty(&existing_val).unwrap()
            }
        }
    }

    #[test]
    fn merge_replaces_only_listed_path() {
        let existing = r#"{
  "name": "demo",
  "scripts": {
    "build": "old-build",
    "test": "vitest"
  }
}"#;
        let incoming = r#"{
  "scripts": {
    "build": "new-build"
  }
}"#;
        let merged = merge(Some(existing), incoming, &["scripts.build"]);
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["scripts"]["build"], Value::String("new-build".into()));
        // Unlisted key under same object survives.
        assert_eq!(v["scripts"]["test"], Value::String("vitest".into()));
        // Top-level untouched.
        assert_eq!(v["name"], Value::String("demo".into()));
    }

    #[test]
    fn merge_creates_intermediate_objects() {
        let existing = r#"{"name": "demo"}"#;
        let incoming = r#"{
  "deps": {
    "serde": "1"
  }
}"#;
        let merged = merge(Some(existing), incoming, &["deps.serde"]);
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["deps"]["serde"], Value::String("1".into()));
        assert_eq!(v["name"], Value::String("demo".into()));
    }

    #[test]
    fn merge_skips_path_missing_from_incoming() {
        let existing = r#"{"deps": {"serde": "1"}}"#;
        let incoming = r#"{"deps": {"clap": "4"}}"#;
        let merged = merge(Some(existing), incoming, &["deps.serde"]);
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["deps"]["serde"], Value::String("1".into()));
        // The lookup didn't include deps.clap, so existing stays.
        assert!(v["deps"].get("clap").is_none());
    }

    #[test]
    fn merge_does_not_touch_unlisted_paths() {
        let existing = r#"{"a": {"keep": 1}, "b": {"also_keep": 2}}"#;
        let incoming = r#"{"a": {"keep": 99}, "b": {"also_keep": 88}}"#;
        let merged = merge(Some(existing), incoming, &["a.keep"]);
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["a"]["keep"], Value::Number(99.into()));
        assert_eq!(v["b"]["also_keep"], Value::Number(2.into()));
    }

    #[test]
    fn merge_refuses_to_clobber_non_object_intermediate() {
        // `package` exists as a string. Path `package.name` would
        // need to walk into a non-object — set_at_path bails out.
        let existing = r#"{"package": "as-a-string"}"#;
        let incoming = r#"{"package": {"name": "new"}}"#;
        let merged = merge(Some(existing), incoming, &["package.name"]);
        let v: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["package"], Value::String("as-a-string".into()));
    }

    #[test]
    fn merge_preserves_key_insertion_order() {
        // serde_json with `preserve_order` keeps the consumer's
        // chosen key order even after a leaf-replace. This is the
        // same property merge-toml uses toml_edit for.
        let existing = r#"{"first": 1, "edited": "old", "last": 3}"#;
        let incoming = r#"{"edited": "new"}"#;
        let merged = merge(Some(existing), incoming, &["edited"]);
        // The order of `first`, `edited`, `last` should be preserved
        // when re-serialised.
        let first_idx = merged.find("\"first\"").unwrap();
        let edited_idx = merged.find("\"edited\"").unwrap();
        let last_idx = merged.find("\"last\"").unwrap();
        assert!(
            first_idx < edited_idx && edited_idx < last_idx,
            "key order changed across re-serialise: {merged}"
        );
    }
}
