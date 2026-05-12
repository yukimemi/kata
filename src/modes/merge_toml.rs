//! `how = "merge-toml"` — keep just the listed paths inside an
//! existing TOML file in sync with the template, leaving every
//! other key, key order, comment, and whitespace alone.
//!
//! Manifest:
//! ```toml
//! [[file]]
//! src   = "Cargo.toml"
//! how   = "merge-toml"
//! when  = "always"
//! paths = ["dependencies.serde", "package.rust-version"]
//! ```
//!
//! For every dotted `paths` entry kata copies the value at that
//! path from the template-rendered body into the existing file
//! at the same path, creating intermediate tables when needed.
//! A path missing in the incoming body is left **untouched** in
//! the existing file (no implicit prune; that's a deliberate
//! conservative choice).
//!
//! **Path syntax limitation**: `paths` are split on the literal
//! `.` character, so a TOML key whose own name contains a dot
//! (e.g. the quoted form `"my.weird.key"`) is **not** addressable
//! via this mode. The common case `dependencies.serde-derive`
//! works fine because `-` isn't a separator. Files that need to
//! poke into quoted dotted keys should use `merge-section`
//! instead, or wait for a future iteration that takes a
//! TOML-aware path parser.
//!
//! **Regex paths (#62)**: a `paths` entry wrapped in `//...//` is
//! interpreted as a regex against the incoming document's
//! dotted-path keys (rvpm-style). kata walks every dotted path in
//! the incoming body, copies each matching path from incoming to
//! existing, and leaves non-matches alone. Regex and literal
//! entries can mix in the same list. Example:
//!
//! ```toml
//! paths = [
//!     "tasks.default",
//!     "//^tasks\\..+$//",   # sweep every tasks.* without enumerating
//! ]
//! ```

use std::path::PathBuf;

use async_trait::async_trait;
use toml_edit::{DocumentMut, Item, Table};

use super::merge_path::{PathSpec, parse_path_spec, shallowest_matches};

use crate::error::{Error, Result};

use super::{
    ActionContext, ActionOutcome, ActionPlan, ApplyMode, OutcomeKind, PlanKind, unified_diff,
};

pub struct MergeToml;

#[async_trait]
impl ApplyMode for MergeToml {
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

    // No existing file → write the template body as-is. The author
    // is bootstrapping; there's nothing to merge into.
    let existing = match ctx.current_body.as_deref() {
        None => return Ok(ctx.rendered_body.clone()),
        Some(s) => s,
    };

    let mut existing_doc: DocumentMut = existing
        .parse()
        .map_err(|e| Error::Merge(format!("merge-toml: parsing existing {}: {e}", ctx.dst_abs)))?;
    let incoming_doc: DocumentMut = ctx.rendered_body.parse().map_err(|e| {
        Error::Merge(format!(
            "merge-toml: parsing incoming for {}: {e}",
            ctx.dst_abs
        ))
    })?;

    // Cache every dotted path present in the incoming doc once —
    // it's needed any time a regex spec appears, and re-collecting
    // per regex spec would be wasteful. `OnceCell`-style lazy init
    // keeps the literal-only fast path zero-cost.
    let mut incoming_paths: Option<Vec<String>> = None;

    for path_str in paths {
        match parse_path_spec(path_str)? {
            PathSpec::Literal(lit) => {
                copy_one_path(&mut existing_doc, &incoming_doc, &lit)?;
            }
            PathSpec::Regex(re) => {
                let collected = incoming_paths.get_or_insert_with(|| {
                    let mut out = Vec::new();
                    collect_dotted_paths(incoming_doc.as_item(), "", &mut out);
                    out
                });
                // Drop child paths when an ancestor also matches:
                // copying the ancestor already brings the whole
                // subtree, so iterating over the children would
                // re-traverse the same data and (more expensively)
                // re-run `items_equivalent` per leaf — see
                // gemini's #90 review. Ancestor detection uses
                // dotted-prefix comparison with an explicit `.`
                // separator so `tasks` doesn't accidentally swallow
                // `tasks-clean`.
                let to_copy = shallowest_matches(collected, &re);
                for p in &to_copy {
                    copy_one_path(&mut existing_doc, &incoming_doc, p)?;
                }
            }
        }
    }

    Ok(existing_doc.to_string())
}

/// Copy the value at one literal dotted path from `incoming_doc`
/// into `existing_doc`. Empty segments (e.g. trailing dot, leading
/// dot, `a..b`) are an error so the manifest author hears about a
/// malformed path instead of getting a silent no-op. Pure no-ops
/// (incoming and existing already equivalent at the path) skip the
/// assignment entirely — see the kata#34 comment below on why
/// that matters for interleaved consumer keys.
fn copy_one_path(
    existing_doc: &mut DocumentMut,
    incoming_doc: &DocumentMut,
    path_str: &str,
) -> Result<()> {
    // Naive `.`-split — module-level docs spell out the
    // limitation (no TOML-quoted-key awareness). Adding a
    // proper parser is future work.
    let segments: Vec<&str> = path_str.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(Error::Merge(format!(
            "merge-toml: empty segment in path `{path_str}` (e.g. trailing dot)"
        )));
    }
    if let Some(value) = item_at_path(incoming_doc.as_item(), &segments).cloned() {
        // If the existing file already has the same value at
        // this path, skip the assignment entirely. toml_edit's
        // emit after `Table::insert` / value-replace on an
        // existing key can shuffle the entry relative to
        // interleaved consumer keys, even when the value
        // didn't change — kata#34. Comparing serialised forms
        // (rather than `Item` equality) ignores attached decor
        // (comments, blank lines, key style) and catches the
        // pure-no-op case reliably.
        let already_matches = item_at_path(existing_doc.as_item(), &segments)
            .is_some_and(|cur| items_equivalent(cur, &value));
        if !already_matches {
            set_at_path(existing_doc, &segments, value);
        }
    }
    // path absent in incoming → leave existing untouched
    Ok(())
}

/// Recursively collect every dotted path in `item`, recording both
/// intermediate tables (so a regex like `^tasks$` can hit the
/// `tasks` super-key) and their leaves (so `^tasks\..+$` works).
/// `prefix` is the dotted-path traversed so far ("" at top level).
fn collect_dotted_paths(item: &Item, prefix: &str, out: &mut Vec<String>) {
    let Some(table) = item.as_table() else {
        return;
    };
    for (key, value) in table.iter() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        out.push(path.clone());
        if value.is_table() {
            collect_dotted_paths(value, &path, out);
        }
    }
}

/// Compare two `Item`s for the kata#34 "skip if no-op" gate.
/// `toml_edit::Item: PartialEq` is decor-aware (table headers,
/// spans, attached comments) so plain `==` reports unequal for
/// items that are semantically identical but parsed from differently
/// formatted source. We want skip-on-true to be lenient: ANY
/// reasonable definition of "same value" should suppress the
/// position-shuffle.
///
/// Implementation: serialise each side as the value half of a
/// sentinel assignment via a throwaway document and compare the
/// canonical bytes. This drops the **key-side** decor (the comments
/// and blank lines that lead into the `[tasks.foo]` header in the
/// original document) but preserves any decor attached to the
/// **value side** (e.g. a trailing `# pin` comment on the value
/// itself), because toml_edit serialises that out. That's the
/// intended sensitivity: if the value has an attached comment in
/// only one of the two sides, the rendered bytes will differ in
/// that comment, so we DO write — keeping the consumer's trailing
/// comment intact across re-applies (kata#34's no-op skip is for
/// the genuinely-no-change case, not for "values match but only
/// one has a comment").
///
/// The cost is two clone-and-serialise round-trips per path. For
/// the typical kata workload (≤ 30 paths × ≤ 200-line files) this
/// is sub-millisecond. If a future merge-toml-heavy project starts
/// noticing it, converting both sides to `toml::Value` (decor-free
/// by construction) is the next iteration.
fn items_equivalent(a: &Item, b: &Item) -> bool {
    fn canon(item: &Item) -> String {
        let mut doc = DocumentMut::new();
        doc.as_table_mut().insert("v", item.clone());
        doc.to_string()
    }
    canon(a) == canon(b)
}

/// Walk a dotted path through nested `Table` items and return the
/// leaf `Item` (or `None` if any segment is missing or the parent
/// isn't a table). InlineTable values terminate the walk — Phase
/// 2-e1 doesn't descend into them; if your path needs to point at
/// a key inside an inline table, restructure the manifest path or
/// switch the file to expanded `[table]` form.
fn item_at_path<'a>(item: &'a Item, path: &[&str]) -> Option<&'a Item> {
    let mut current = item;
    for seg in path {
        match current {
            Item::Table(t) => current = t.get(seg)?,
            // ArrayOfTables / InlineTable / scalar — not a path we
            // can keep walking through.
            _ => return None,
        }
    }
    Some(current)
}

/// Set the value at a dotted path, creating intermediate
/// **missing** `Table`s as needed. If any intermediate slot is
/// already occupied by something *other than* a table the call
/// is a silent no-op — kata refuses to clobber unrelated
/// structure to keep `merge-toml` strictly additive on the
/// listed paths.
fn set_at_path(doc: &mut DocumentMut, path: &[&str], value: Item) {
    if path.is_empty() {
        return;
    }

    let mut current: &mut Item = doc.as_item_mut();
    for &seg in &path[..path.len() - 1] {
        // Refuse to overwrite a non-table intermediate. If the
        // existing file has e.g. `package = "..."` and the path
        // tries to reach `package.foo`, leave the file alone.
        if !current.is_table() {
            return;
        }
        let table = current.as_table_mut().expect("just ensured table above");
        current = table
            .entry(seg)
            .or_insert_with(|| Item::Table(Table::new()));
    }

    if let Some(table) = current.as_table_mut() {
        let last = path.last().expect("path is non-empty");
        // Update in place when the key already exists: `Table::insert`
        // on an existing key may shuffle the entry's position relative
        // to interleaved consumer keys (yukimemi/kata#34). Assigning
        // through `get_mut` replaces the value but preserves position
        // and surrounding decor (comments, blank lines). Falls back
        // to `insert` only when the key is genuinely new.
        if let Some(existing) = table.get_mut(last) {
            *existing = value;
        } else {
            table.insert(last, value);
        }
    }
}

fn require_paths<'a>(ctx: &'a ActionContext<'_>) -> Result<&'a Vec<String>> {
    if ctx.spec.paths.is_empty() {
        return Err(Error::manifest(
            PathBuf::from(&ctx.template.source_spec),
            format!(
                "how=\"merge-toml\" requires `paths = [...]` in `[[file]]` for {}",
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
        let paths_owned: Vec<String> = paths.iter().map(|s| s.to_string()).collect();

        match existing {
            None => incoming.to_string(),
            Some(existing) => {
                let mut existing_doc: DocumentMut = existing.parse().unwrap();
                let incoming_doc: DocumentMut = incoming.parse().unwrap();
                let mut incoming_paths: Option<Vec<String>> = None;
                for path_str in &paths_owned {
                    match parse_path_spec(path_str).unwrap() {
                        PathSpec::Literal(lit) => {
                            copy_one_path(&mut existing_doc, &incoming_doc, &lit).unwrap();
                        }
                        PathSpec::Regex(re) => {
                            let collected = incoming_paths.get_or_insert_with(|| {
                                let mut out = Vec::new();
                                collect_dotted_paths(incoming_doc.as_item(), "", &mut out);
                                out
                            });
                            for p in &shallowest_matches(collected, &re) {
                                copy_one_path(&mut existing_doc, &incoming_doc, p).unwrap();
                            }
                        }
                    }
                }
                existing_doc.to_string()
            }
        }
    }

    #[test]
    fn merge_replaces_only_listed_path() {
        let existing = "\
# header comment
[package]
name = \"demo\"

[dependencies]
serde = \"1.0.180\"          # old version
clap  = \"4.5\"              # don't touch me
";
        let incoming = "\
[package]
name = \"demo\"

[dependencies]
serde = \"1.0.220\"
";
        let merged = merge(Some(existing), incoming, &["dependencies.serde"]);

        // serde version updated …
        assert!(
            merged.contains("serde = \"1.0.220\""),
            "serde should be updated: {merged}"
        );
        // … clap line preserved verbatim with its comment …
        assert!(
            merged.contains("clap  = \"4.5\"              # don't touch me"),
            "clap line + trailing comment must be preserved: {merged}"
        );
        // … and the header comment too.
        assert!(merged.starts_with("# header comment\n"));
    }

    #[test]
    fn merge_creates_intermediate_tables() {
        let existing = "[package]\nname = \"demo\"\n";
        let incoming = "\
[package]
name = \"demo\"

[dependencies]
serde = \"1\"
";
        let merged = merge(Some(existing), incoming, &["dependencies.serde"]);
        assert!(merged.contains("[dependencies]"));
        assert!(merged.contains("serde = \"1\""));
        assert!(merged.contains("name = \"demo\""));
    }

    #[test]
    fn merge_skips_path_missing_from_incoming() {
        let existing = "[deps]\nserde = \"1\"\n";
        let incoming = "[deps]\nclap = \"4\"\n"; // no serde
        let merged = merge(Some(existing), incoming, &["deps.serde"]);
        // existing serde stays put …
        assert!(merged.contains("serde = \"1\""));
        // … and we didn't accidentally append clap.
        assert!(!merged.contains("clap"));
    }

    #[test]
    fn merge_does_not_touch_unlisted_paths() {
        let existing = "\
[a]
keep = 1

[b]
also_keep = 2
";
        let incoming = "\
[a]
keep = 99

[b]
also_keep = 88
";
        let merged = merge(Some(existing), incoming, &["a.keep"]);
        assert!(merged.contains("keep = 99")); // listed path updated
        assert!(merged.contains("also_keep = 2")); // unlisted preserved
    }

    #[test]
    fn merge_creates_full_file_when_dst_absent() {
        let incoming = "[package]\nname = \"x\"\n";
        let merged = merge(None, incoming, &["package.name"]);
        assert_eq!(merged, incoming);
    }

    #[test]
    fn merge_is_idempotent_with_interleaved_consumer_keys() {
        // Regression for yukimemi/kata#34. Consumer-specific tasks
        // sitting **between** kata-managed tasks must keep their
        // position across re-applies. Before the fix, toml_edit's
        // mid-loop `Table::insert` shuffled the interleaved keys
        // every apply — `kata status` always reported drift even
        // when nothing semantic changed.
        let existing = "\
[tasks.check]
deps = [\"fmt-check\", \"clippy\", \"test\"]

[tasks.clippy-none]
# consumer-specific task, MUST stay between clippy and test
desc = \"clippy with --no-default-features\"

[tasks.clippy]
args = [\"clippy\", \"--all-targets\"]

[tasks.test-all]
# another consumer task interleaved deeper in
desc = \"run all tests\"

[tasks.test]
args = [\"test\", \"--all-targets\"]
";
        let incoming = "\
[tasks.check]
deps = [\"fmt-check\", \"clippy\", \"test\"]

[tasks.clippy]
args = [\"clippy\", \"--all-targets\", \"--\", \"-D\", \"warnings\"]

[tasks.test]
args = [\"test\", \"--all-targets\"]
";
        let paths = &["tasks.check", "tasks.clippy", "tasks.test"];
        let first = merge(Some(existing), incoming, paths);
        let second = merge(Some(&first), incoming, paths);
        assert_eq!(
            first, second,
            "merge must be idempotent across re-applies — drift\n\
             on a no-op merge is yukimemi/kata#34.\n\
             first:\n{first}\nsecond:\n{second}",
        );
        // And the consumer tasks must still be present (no
        // regression of the earlier destructive-merge fix).
        assert!(first.contains("clippy-none"), "consumer task lost: {first}");
        assert!(first.contains("test-all"), "consumer task lost: {first}");
    }

    #[test]
    fn merge_refuses_to_clobber_non_table_intermediate() {
        // `package` exists as a STRING in the existing file. The
        // path `package.name` tries to walk into a parent that
        // isn't a table — set_at_path must bail out, leaving the
        // string untouched (no silent overwrite, no panic).
        let existing = "package = \"as-a-string\"\n";
        let incoming = "[package]\nname = \"new\"\n";
        let merged = merge(Some(existing), incoming, &["package.name"]);
        // existing was preserved, no clobber
        assert!(
            merged.contains("package = \"as-a-string\""),
            "non-table intermediate must NOT be clobbered: {merged}"
        );
        // and we didn't accidentally create [package].name
        assert!(
            !merged.contains("[package]") && !merged.contains("name = \"new\""),
            "no fresh [package] table should appear: {merged}"
        );
    }

    #[test]
    fn regex_path_sweeps_all_tasks_subkeys() {
        // Issue #62 motivating case: pj-rust's Makefile.toml ships
        // tasks.{default,check,fmt-check,fmt,clippy,test,
        // test-targets,test-doc,lock-check,...}. Listing each name
        // by hand is error-prone — every new sub-task added
        // upstream needs an explicit append to the consumer's
        // `paths`. A single `//^tasks\..+$//` regex sweeps the
        // entire subtree.
        let existing = "\
[tasks.default]
deps = [\"old\"]

[tasks.test]
args = [\"old-args\"]
";
        let incoming = "\
[tasks.default]
deps = [\"check\"]

[tasks.test]
args = [\"test\", \"--all-targets\"]

[tasks.test-doc]
args = [\"test\", \"--doc\"]
";
        let merged = merge(Some(existing), incoming, &[r"//^tasks\..+$//"]);
        assert!(
            merged.contains("deps = [\"check\"]"),
            "regex must update tasks.default: {merged}"
        );
        assert!(
            merged.contains("test-doc") && merged.contains("--doc"),
            "regex must also pull in tasks.test-doc (new sub-key): {merged}"
        );
    }

    #[test]
    fn regex_and_literal_paths_compose() {
        // A regex and literal entries in the same `paths` list
        // should both fire. The literal-only path should remain
        // unaffected by regex matches that don't cover it.
        let existing = "\
[a]
keep_a = 1

[b]
keep_b = 2
";
        let incoming = "\
[a]
keep_a = 99

[b]
keep_b = 88
nested = \"new\"
";
        let merged = merge(Some(existing), incoming, &["a.keep_a", r"//^b\..+$//"]);
        assert!(merged.contains("keep_a = 99"), "literal: {merged}");
        assert!(merged.contains("keep_b = 88"), "regex hit keep_b: {merged}");
        assert!(
            merged.contains("nested = \"new\""),
            "regex hit nested: {merged}"
        );
    }

    #[test]
    fn regex_skips_paths_not_in_incoming() {
        // A regex matches the names of keys that EXIST in the
        // incoming body. The existing file may have keys the
        // regex would also match if it appeared in incoming, but
        // those stay untouched (same "no implicit prune" rule as
        // literal paths).
        let existing = "\
[tasks.only_in_existing]
note = \"keep\"
";
        let incoming = "\
[tasks.only_in_incoming]
note = \"add\"
";
        let merged = merge(Some(existing), incoming, &[r"//^tasks\..+$//"]);
        assert!(
            merged.contains("only_in_existing") && merged.contains("note = \"keep\""),
            "existing-only key must survive regex sweep: {merged}"
        );
        assert!(
            merged.contains("only_in_incoming") && merged.contains("note = \"add\""),
            "incoming-only key (matching regex) must be added: {merged}"
        );
    }
}
