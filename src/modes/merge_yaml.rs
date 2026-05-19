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
//!
//! **Sequence indexing (#107 follow-up)**: a segment of the form
//! `name[idx]` addresses element `idx` of a YAML sequence at key
//! `name`. So `servers[0]` lets the template own only the first
//! element of a `servers:` list while consumers freely append more.
//! Bootstrap matches `merge-toml`: if the existing file has no
//! `name` entry at all (or an empty sequence), kata creates /
//! pushes the index-0 element only. Out-of-range indices on an
//! existing non-empty sequence are a silent no-op rather than
//! padding null entries.
//!
//! Unlike TOML's `ArrayOfTables` (table-only elements), YAML
//! sequences are uniform — `tags[0]` works for `tags: [foo, bar]`
//! too, replacing the string at index 0.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_yaml::{Mapping, Value};

use crate::error::{Error, Result};

use super::merge_path::{PathSeg, PathSpec, parse_path_spec, parse_segments, shallowest_matches};
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
/// `name[idx]` segments address sequence elements (yukimemi/kata
/// #107 follow-up).
fn copy_one_path(existing_val: &mut Value, incoming_val: &Value, path_str: &str) -> Result<()> {
    let segments =
        parse_segments(path_str).map_err(|e| Error::Merge(format!("merge-yaml: {e}")))?;
    if segments.is_empty() {
        return Ok(());
    }
    if let Some(value) = value_at_path(incoming_val, &segments) {
        set_at_path(existing_val, &segments, value);
    }
    Ok(())
}

/// Recursively enumerate every dotted path in `val` — intermediate
/// mappings AND their leaves, plus `prefix.key[N]` paths for
/// sequence elements (#107 follow-up). Mirrors
/// `merge-toml::collect_dotted_paths` for the YAML data model.
fn collect_dotted_paths(val: &Value, prefix: &str, out: &mut Vec<String>) {
    match val {
        Value::Mapping(map) => collect_in_mapping(map, prefix, out),
        // Skip root-level sequences. `parse_segments` rejects the
        // bare `[N]` form (empty name before `[`), and
        // `value_at_path` / `set_at_path` both assume the root is a
        // mapping anyway. A `prefix` only becomes non-empty after
        // we've recursed through at least one key, so this guard
        // matches that invariant. See Gemini #110 review.
        Value::Sequence(seq) if !prefix.is_empty() => {
            for (idx, elem) in seq.iter().enumerate() {
                let path = format!("{prefix}[{idx}]");
                out.push(path.clone());
                collect_dotted_paths(elem, &path, out);
            }
        }
        // Root-level sequence, scalars, tagged values — no walk.
        _ => {}
    }
}

fn collect_in_mapping(map: &Mapping, prefix: &str, out: &mut Vec<String>) {
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
        collect_dotted_paths(value, &path, out);
    }
}

/// Walk a path through nested `Mapping`s / `Sequence`s and return
/// the leaf `Value`, cloned (caller wants ownership for assignment).
/// Returns `None` if any segment is missing, the parent shape
/// doesn't match the segment kind (`Key` against a sequence, or
/// `KeyIndex` against a mapping / scalar), or the index is out of
/// range.
fn value_at_path(val: &Value, path: &[PathSeg]) -> Option<Value> {
    if path.is_empty() {
        return Some(val.clone());
    }
    let map = val.as_mapping()?;
    value_at_mapping_path(map, path)
}

fn value_at_mapping_path(map: &Mapping, path: &[PathSeg]) -> Option<Value> {
    let (head, rest) = path.split_first().expect("caller checks non-empty");
    match head {
        PathSeg::Key(k) => {
            let next = map.get(k.as_str())?;
            if rest.is_empty() {
                return Some(next.clone());
            }
            value_at_mapping_path(next.as_mapping()?, rest)
        }
        PathSeg::KeyIndex(k, i) => {
            let seq = map.get(k.as_str())?.as_sequence()?;
            let elem = seq.get(*i)?;
            if rest.is_empty() {
                return Some(elem.clone());
            }
            value_at_mapping_path(elem.as_mapping()?, rest)
        }
    }
}

/// Set the value at a path, creating intermediate **missing**
/// `Mapping`s and bootstrap `Sequence`s (index 0 only) as needed.
/// Refuses to clobber a slot already holding a wrong-shape value
/// (`Key` step into a non-mapping, `KeyIndex` step into a
/// non-sequence, or out-of-range index on a non-empty existing
/// sequence) — same conservative contract `merge-toml` enforces.
fn set_at_path(val: &mut Value, path: &[PathSeg], value: Value) {
    let Some(map) = val.as_mapping_mut() else {
        return;
    };
    set_in_mapping(map, path, value);
}

fn set_in_mapping(map: &mut Mapping, path: &[PathSeg], value: Value) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    let is_leaf = rest.is_empty();
    match head {
        PathSeg::Key(k) => {
            if is_leaf {
                map.insert(Value::String(k.clone()), value);
                return;
            }
            if !map.contains_key(k.as_str()) {
                map.insert(Value::String(k.clone()), Value::Mapping(Mapping::new()));
            }
            let next = map.get_mut(k.as_str()).expect("just ensured present");
            let Some(next_map) = next.as_mapping_mut() else {
                return; // refuse to clobber a non-mapping intermediate
            };
            set_in_mapping(next_map, rest, value);
        }
        PathSeg::KeyIndex(k, i) => {
            if is_leaf {
                if let Some(elem) = ensure_seq_element(map, k, *i) {
                    *elem = value;
                }
                return;
            }
            // Intermediate KeyIndex: the existing element must be
            // a mapping (or we can bootstrap it as one) to keep
            // descending. ensure_seq_element gives us the slot;
            // if it isn't a mapping we refuse to clobber.
            let Some(elem) = ensure_seq_element(map, k, *i) else {
                return;
            };
            if elem.is_null() {
                // ensure_seq_element bootstrapped a fresh slot
                // (sequence was missing or had len==0). Use a
                // mapping there so the next step can descend.
                *elem = Value::Mapping(Mapping::new());
            }
            let Some(elem_map) = elem.as_mapping_mut() else {
                return;
            };
            set_in_mapping(elem_map, rest, value);
        }
    }
}

/// Ensure `map[k][i]` exists and return `&mut Value` to it, or
/// `None` if the conservative rule says skip:
///
/// - key missing AND `i != 0` → no-op (don't pad with nulls)
/// - key missing AND `i == 0` → create `Value::Sequence(vec![Null])`
///   and return a borrow to slot 0 (caller decides what to write)
/// - key present, not a sequence → no-op (refuse-to-clobber)
/// - key present, empty sequence AND `i == 0` → push a `Null` slot
///   and return a borrow to it
/// - key present, `i < len` → return slot `i`
/// - any other out-of-range case → no-op
fn ensure_seq_element<'a>(map: &'a mut Mapping, k: &str, i: usize) -> Option<&'a mut Value> {
    if !map.contains_key(k) {
        if i != 0 {
            return None;
        }
        map.insert(
            Value::String(k.to_string()),
            Value::Sequence(vec![Value::Null]),
        );
        return map.get_mut(k)?.as_sequence_mut()?.get_mut(0);
    }
    let entry = map.get_mut(k)?;
    let seq = entry.as_sequence_mut()?;
    if seq.is_empty() {
        if i != 0 {
            return None;
        }
        seq.push(Value::Null);
        return seq.get_mut(0);
    }
    if i >= seq.len() {
        return None;
    }
    seq.get_mut(i)
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
    fn merge_replaces_only_index_zero_of_sequence_of_mappings() {
        // Mirror of merge-toml's #107 motivating case for YAML's
        // sequence-of-mappings form. Template owns servers[0], the
        // consumer keeps the second entry.
        let existing = "\
servers:
  - name: kata-managed
    port: 8080
  - name: consumer-added
    port: 9090
";
        let incoming = "\
servers:
  - name: kata-managed
    port: 443
    tls: true
";
        let merged = merge(Some(existing), incoming, &["servers[0]"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["servers"][0]["port"], Value::Number(443.into()));
        assert_eq!(v["servers"][0]["tls"], Value::Bool(true));
        assert_eq!(
            v["servers"][1]["name"],
            Value::String("consumer-added".into()),
            "consumer's second element must survive: {merged}"
        );
    }

    #[test]
    fn merge_bootstraps_sequence_when_missing() {
        // Existing has no `servers` key at all. Path `servers[0]`
        // should create the sequence and seed the first element.
        let existing = "project:\n  name: x\n";
        let incoming = "\
servers:
  - name: a
    port: 80
";
        let merged = merge(Some(existing), incoming, &["servers[0]"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["servers"][0]["name"], Value::String("a".into()));
        assert_eq!(v["servers"][0]["port"], Value::Number(80.into()));
        assert_eq!(v["project"]["name"], Value::String("x".into()));
    }

    #[test]
    fn merge_skips_out_of_range_index_on_shorter_sequence() {
        // Existing has 1 element; path asks for index 1. Conservative:
        // don't pad, leave the sequence as-is.
        let existing = "\
servers:
  - name: keep
    port: 1
";
        let incoming = "\
servers:
  - name: first
  - name: second
";
        let merged = merge(Some(existing), incoming, &["servers[1]"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(
            v["servers"].as_sequence().unwrap().len(),
            1,
            "must not pad: {merged}"
        );
        assert_eq!(v["servers"][0]["name"], Value::String("keep".into()));
    }

    #[test]
    fn merge_refuses_to_clobber_non_sequence_at_index_path() {
        // Existing has `servers` as a mapping (wrong shape). Path
        // `servers[0]` must NOT clobber the mapping.
        let existing = "\
servers:
  not-a-sequence: true
";
        let incoming = "\
servers:
  - name: a
";
        let merged = merge(Some(existing), incoming, &["servers[0]"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        // mapping survives — no sequence created.
        assert_eq!(v["servers"]["not-a-sequence"], Value::Bool(true));
        assert!(
            !v["servers"].is_sequence(),
            "non-sequence must NOT be clobbered: {merged}"
        );
    }

    #[test]
    fn merge_can_address_field_inside_sequence_element() {
        // `servers[0].name` replaces only the `name` of element 0,
        // leaving sibling keys (and other elements) alone.
        let existing = "\
servers:
  - name: old
    port: keep
  - name: consumer
";
        let incoming = "\
servers:
  - name: new
    port: replaced
";
        let merged = merge(Some(existing), incoming, &["servers[0].name"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["servers"][0]["name"], Value::String("new".into()));
        // port was NOT in the path → keep existing
        assert_eq!(v["servers"][0]["port"], Value::String("keep".into()));
        // element 1 untouched
        assert_eq!(v["servers"][1]["name"], Value::String("consumer".into()));
    }

    #[test]
    fn merge_can_replace_scalar_sequence_element() {
        // YAML sequences are uniform — `tags[0]` should work even
        // when the element is a string, not a mapping.
        let existing = "tags:\n  - old\n  - keep\n";
        let incoming = "tags:\n  - new\n";
        let merged = merge(Some(existing), incoming, &["tags[0]"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["tags"][0], Value::String("new".into()));
        assert_eq!(v["tags"][1], Value::String("keep".into()));
    }

    #[test]
    fn merge_sequence_index_is_idempotent() {
        let existing = "\
servers:
  - name: a
    port: 1
  - name: consumer
";
        let incoming = "\
servers:
  - name: a
    port: 1
";
        let first = merge(Some(existing), incoming, &["servers[0]"]);
        let second = merge(Some(&first), incoming, &["servers[0]"]);
        assert_eq!(first, second, "sequence-index merge must be idempotent");
    }

    #[test]
    fn regex_can_target_specific_sequence_element() {
        let existing = "\
servers:
  - name: old
  - name: consumer
";
        let incoming = "\
servers:
  - name: new
";
        let merged = merge(Some(existing), incoming, &[r"//^servers\[0\]$//"]);
        let v: Value = serde_yaml::from_str(&merged).unwrap();
        assert_eq!(v["servers"][0]["name"], Value::String("new".into()));
        assert_eq!(v["servers"][1]["name"], Value::String("consumer".into()));
    }

    #[test]
    fn collect_dotted_paths_skips_root_level_sequence() {
        // Gemini #110 regression: a YAML document whose root is a
        // sequence must NOT have `[0]`, `[1]`, ... emitted as
        // top-level paths — `parse_segments` rejects the bare
        // bracket form (empty name before `[`) and
        // `value_at_path` / `set_at_path` both require the root to
        // be a mapping. Skipping aligns the path enumeration with
        // those invariants instead of synthesising unparseable
        // paths.
        let val: Value = serde_yaml::from_str("- name: a\n- name: b\n").unwrap();
        let mut paths = Vec::new();
        collect_dotted_paths(&val, "", &mut paths);
        assert!(
            paths.is_empty(),
            "root-level sequence must not emit paths: {paths:?}"
        );
    }

    #[test]
    fn collect_dotted_paths_emits_sequence_index_forms() {
        let val: Value = serde_yaml::from_str(
            "\
servers:
  - name: a
  - name: b
",
        )
        .unwrap();
        let mut paths = Vec::new();
        collect_dotted_paths(&val, "", &mut paths);
        assert!(paths.iter().any(|p| p == "servers"));
        assert!(paths.iter().any(|p| p == "servers[0]"));
        assert!(paths.iter().any(|p| p == "servers[1]"));
        assert!(
            paths.iter().any(|p| p == "servers[0].name"),
            "inside-element path: {paths:?}"
        );
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
