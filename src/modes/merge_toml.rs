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
//! **Array-of-tables indexing (#107)**: a segment of the form
//! `name[idx]` addresses element `idx` of an `[[name]]` array of
//! tables, leaving the other elements untouched. So
//! `hooks.post_create[0]` lets the upstream template own only the
//! first hook and the consumer freely append further
//! `[[hooks.post_create]]` entries. Bootstrap: when the existing
//! file has no `name` entry at all (or has it as an empty
//! `ArrayOfTables`), kata creates / extends to index 0 only.
//! Beyond that the conservative rule applies — if `idx >= len` for
//! an existing non-empty array, the path is a silent no-op rather
//! than padding empty tables.
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
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use super::merge_path::{PathSeg, PathSpec, parse_path_spec, parse_segments, shallowest_matches};

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
/// dot, `a..b`) and malformed `name[idx]` brackets are errors so
/// the manifest author hears about a malformed path instead of
/// getting a silent no-op. Pure no-ops (incoming and existing
/// already equivalent at the path) skip the assignment entirely —
/// see the kata#34 comment below on why that matters for
/// interleaved consumer keys.
fn copy_one_path(
    existing_doc: &mut DocumentMut,
    incoming_doc: &DocumentMut,
    path_str: &str,
) -> Result<()> {
    let segments =
        parse_segments(path_str).map_err(|e| Error::Merge(format!("merge-toml: {e}")))?;
    if segments.is_empty() {
        return Ok(());
    }
    if let Some(value) = item_at_path(incoming_doc.as_item(), &segments) {
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
            .as_ref()
            .is_some_and(|cur| items_equivalent(cur, &value));
        if !already_matches {
            set_at_path(existing_doc, &segments, value);
        }
    }
    // path absent in incoming → leave existing untouched
    Ok(())
}

/// Recursively collect every dotted path in `item`, recording
/// intermediate tables (so a regex like `^tasks$` can hit the
/// super-key), their leaves, and — for `[[name]]` array-of-tables
/// entries — the per-element `name[idx]` paths plus everything
/// inside each element table (yukimemi/kata#107). `prefix` is the
/// dotted-path traversed so far ("" at top level).
fn collect_dotted_paths(item: &Item, prefix: &str, out: &mut Vec<String>) {
    match item {
        Item::Table(table) => collect_in_table(table, prefix, out),
        Item::ArrayOfTables(aot) => {
            for (idx, elem) in aot.iter().enumerate() {
                let path = format!("{prefix}[{idx}]");
                out.push(path.clone());
                // `elem` is `&Table`, so recurse via the borrowed
                // helper — no `Item::Table(elem.clone())` lift
                // (Gemini #108 review).
                collect_in_table(elem, &path, out);
            }
        }
        // Inline values, inline tables, value arrays — no further
        // walk (same as pre-#107).
        _ => {}
    }
}

/// Borrowed-table walker — counterpart to `collect_dotted_paths`
/// that avoids the per-element clone an `Item::Table`-wrapped
/// recursion would incur. `Item::Table` dispatches here; the
/// `ArrayOfTables` arm above calls in directly on each element
/// `&Table`.
fn collect_in_table(table: &Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table.iter() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        out.push(path.clone());
        collect_dotted_paths(value, &path, out);
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

/// Walk a path through nested `Table` / `ArrayOfTables` items and
/// return the leaf `Item` (cloned — caller wants to assign it
/// elsewhere). Returns `None` if any segment is missing, the
/// parent shape doesn't match the segment kind (e.g. `Key`
/// against an `ArrayOfTables`, or `KeyIndex` against a scalar),
/// or the index is out of range.
///
/// Intermediate descents go through the borrowed-table helper
/// `item_at_table_path` so an `ArrayOfTables` step doesn't clone
/// the whole element per recursion — only the final leaf is
/// cloned (Gemini #108 review).
///
/// `InlineTable` values terminate the walk — Phase 2-e1 doesn't
/// descend into them; if a path needs to point at a key inside an
/// inline table, restructure the manifest path or switch the file
/// to expanded `[table]` form.
fn item_at_path(item: &Item, path: &[PathSeg]) -> Option<Item> {
    if path.is_empty() {
        return Some(item.clone());
    }
    item_at_table_path(item.as_table()?, path)
}

/// Borrowed-table walker. Descends through `&Table` /
/// `ArrayOfTables → &Table` without cloning intermediates, then
/// clones at the leaf so the returned `Item` can be assigned
/// elsewhere by the caller.
fn item_at_table_path(table: &Table, path: &[PathSeg]) -> Option<Item> {
    let (head, rest) = path.split_first().expect("caller checks non-empty");
    match head {
        PathSeg::Key(k) => {
            let next = table.get(k)?;
            if rest.is_empty() {
                return Some(next.clone());
            }
            // Only `Table` continues the walk: `ArrayOfTables`,
            // inline tables, and scalars all terminate (the
            // remaining segments can't address through them
            // because `PathSeg::Key` after an array would need a
            // `KeyIndex` first).
            item_at_table_path(next.as_table()?, rest)
        }
        PathSeg::KeyIndex(k, i) => {
            let aot = table.get(k)?.as_array_of_tables()?;
            let elem = aot.get(*i)?;
            if rest.is_empty() {
                return Some(Item::Table(elem.clone()));
            }
            item_at_table_path(elem, rest)
        }
    }
}

/// Set the value at a path, creating intermediate **missing**
/// `Table`s and bootstrap `ArrayOfTables` (index 0 only) as needed.
/// Refuses to clobber slots that already hold a wrong-shape item:
/// a `Key` step against an existing non-table, a `KeyIndex` step
/// against an existing non-array-of-tables, or a `KeyIndex` with
/// out-of-range index on a non-empty existing array all silently
/// no-op rather than rewriting unrelated structure.
fn set_at_path(doc: &mut DocumentMut, path: &[PathSeg], value: Item) {
    set_in_table(doc.as_table_mut(), path, value);
}

fn set_in_table(table: &mut Table, path: &[PathSeg], value: Item) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    let is_leaf = rest.is_empty();
    match head {
        PathSeg::Key(k) => {
            if is_leaf {
                // Update in place when the key already exists:
                // `Table::insert` on an existing key may shuffle
                // the entry's position relative to interleaved
                // consumer keys (yukimemi/kata#34). Assigning
                // through `get_mut` replaces the value but
                // preserves position and surrounding decor.
                if let Some(existing) = table.get_mut(k) {
                    *existing = value;
                } else {
                    table.insert(k, value);
                }
            } else {
                let entry = table.entry(k).or_insert_with(|| Item::Table(Table::new()));
                let Some(next) = entry.as_table_mut() else {
                    return; // existing non-table intermediate — refuse to clobber
                };
                set_in_table(next, rest, value);
            }
        }
        PathSeg::KeyIndex(k, i) => {
            let Some(elem) = ensure_aot_element(table, k, *i) else {
                return;
            };
            if is_leaf {
                // `ArrayOfTables` only holds `Table` values. If the
                // incoming item isn't a table (e.g. someone aimed
                // an array-index path at a scalar value in the
                // template), refuse rather than synthesise a stray
                // shape.
                let Item::Table(value_table) = value else {
                    return;
                };
                *elem = value_table;
            } else {
                set_in_table(elem, rest, value);
            }
        }
    }
}

/// Ensure element `idx` of `table.entry(key)` exists as a `&mut
/// Table` and return a borrow to it, or `None` if the conservative
/// rule says skip:
///
/// - key missing AND `idx != 0` → no-op (don't pad)
/// - key missing AND `idx == 0` → bootstrap empty `ArrayOfTables`
///   with one fresh `Table` and return it
/// - key present, not an `ArrayOfTables` → no-op (refuse to
///   clobber unrelated structure, same contract as `Key`'s table
///   intermediate)
/// - key present, empty array AND `idx == 0` → push one fresh
///   `Table` and return it (bootstrap of an already-emptied array)
/// - key present, `idx < len` → return element `idx`
/// - any other out-of-range case → no-op
fn ensure_aot_element<'a>(table: &'a mut Table, key: &str, idx: usize) -> Option<&'a mut Table> {
    if !table.contains_key(key) {
        if idx != 0 {
            return None;
        }
        let mut aot = ArrayOfTables::new();
        aot.push(Table::new());
        table.insert(key, Item::ArrayOfTables(aot));
        return table.get_mut(key)?.as_array_of_tables_mut()?.get_mut(0);
    }
    let entry = table.get_mut(key)?;
    let aot = entry.as_array_of_tables_mut()?;
    if aot.is_empty() {
        if idx != 0 {
            return None;
        }
        aot.push(Table::new());
        return aot.get_mut(0);
    }
    if idx >= aot.len() {
        return None;
    }
    aot.get_mut(idx)
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
    fn merge_replaces_only_index_zero_of_array_of_tables() {
        // yukimemi/kata#107 motivating case: kata owns
        // `hooks.post_create[0]`, the consumer appends a second
        // `[[hooks.post_create]]` for their own SPA install step.
        // Merge must replace the first element only.
        let existing = "\
[[hooks.post_create]]
cmd = \"cargo make on-add\"

[[hooks.post_create]]
cmd = \"bun install --cwd crates/kanade-backend/web\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"cargo make on-add --updated\"
";
        let merged = merge(Some(existing), incoming, &["hooks.post_create[0]"]);
        assert!(
            merged.contains("cargo make on-add --updated"),
            "element 0 must be updated: {merged}"
        );
        assert!(
            merged.contains("bun install --cwd crates/kanade-backend/web"),
            "element 1 (consumer's) must survive: {merged}"
        );
    }

    #[test]
    fn merge_bootstraps_array_of_tables_when_missing() {
        // Consumer doesn't have `[[hooks.post_create]]` at all yet.
        // For idx 0 the path should create the array and seed the
        // first element — same shape as the "missing intermediate
        // table gets created" rule for plain Key paths.
        let existing = "[project]\nname = \"x\"\n";
        let incoming = "\
[[hooks.post_create]]
cmd = \"cargo make on-add\"
";
        let merged = merge(Some(existing), incoming, &["hooks.post_create[0]"]);
        assert!(
            merged.contains("[[hooks.post_create]]"),
            "missing array must be bootstrapped: {merged}"
        );
        assert!(
            merged.contains("cmd = \"cargo make on-add\""),
            "bootstrapped element must carry the value: {merged}"
        );
        // existing keys preserved
        assert!(merged.contains("name = \"x\""));
    }

    #[test]
    fn merge_skips_out_of_range_index_on_shorter_array() {
        // Existing has 1 element; path asks for index 1 (i.e. one
        // past the end). Conservative rule: don't pad, leave the
        // array unchanged.
        let existing = "\
[[hooks.post_create]]
cmd = \"keep\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"first\"

[[hooks.post_create]]
cmd = \"second\"
";
        let merged = merge(Some(existing), incoming, &["hooks.post_create[1]"]);
        // unchanged
        assert!(merged.contains("cmd = \"keep\""));
        assert!(
            !merged.contains("cmd = \"second\""),
            "must not pad / append: {merged}"
        );
    }

    #[test]
    fn merge_skips_index_zero_against_non_array_intermediate() {
        // Existing has `hooks.post_create = "string"` (the wrong
        // shape). Path `hooks.post_create[0]` must NOT clobber the
        // scalar — same refuse-to-clobber rule the Key path uses
        // for non-table intermediates.
        let existing = "\
[hooks]
post_create = \"not-an-array\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"cargo make on-add\"
";
        let merged = merge(Some(existing), incoming, &["hooks.post_create[0]"]);
        assert!(
            merged.contains("post_create = \"not-an-array\""),
            "non-array intermediate must NOT be clobbered: {merged}"
        );
        assert!(
            !merged.contains("[[hooks.post_create]]"),
            "no array form should appear: {merged}"
        );
    }

    #[test]
    fn merge_can_address_field_inside_array_element() {
        // `hooks.post_create[0].cmd` reaches into the first element
        // and replaces just the `cmd` key, leaving sibling keys
        // (and other array elements) alone.
        let existing = "\
[[hooks.post_create]]
cmd = \"old\"
cwd = \"keep\"

[[hooks.post_create]]
cmd = \"consumer\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"new\"
cwd = \"replaced\"
";
        let merged = merge(Some(existing), incoming, &["hooks.post_create[0].cmd"]);
        assert!(merged.contains("cmd = \"new\""), "cmd updated: {merged}");
        // cwd inside element 0 was NOT in paths → preserved.
        assert!(
            merged.contains("cwd = \"keep\""),
            "sibling key inside element 0 preserved: {merged}"
        );
        // Consumer's element 1 untouched.
        assert!(
            merged.contains("cmd = \"consumer\""),
            "element 1 preserved: {merged}"
        );
    }

    #[test]
    fn merge_array_index_path_is_idempotent() {
        // Re-apply must not reshuffle decor (yukimemi/kata#34
        // shape, but for the array path). Two passes produce the
        // same output.
        let existing = "\
[[hooks.post_create]]
cmd = \"cargo make on-add\"

[[hooks.post_create]]
cmd = \"bun install\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"cargo make on-add\"
";
        let first = merge(Some(existing), incoming, &["hooks.post_create[0]"]);
        let second = merge(Some(&first), incoming, &["hooks.post_create[0]"]);
        assert_eq!(
            first, second,
            "merge must be idempotent on array-index paths"
        );
    }

    #[test]
    fn regex_can_target_specific_array_element() {
        // A regex form `//^hooks\\.post_create\\[0\\]$//` should
        // hit only the bracketed path (and let the consumer keep
        // element 1).
        let existing = "\
[[hooks.post_create]]
cmd = \"old\"

[[hooks.post_create]]
cmd = \"consumer\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"new\"
";
        let merged = merge(
            Some(existing),
            incoming,
            &[r"//^hooks\.post_create\[0\]$//"],
        );
        assert!(merged.contains("cmd = \"new\""));
        assert!(
            merged.contains("cmd = \"consumer\""),
            "consumer element survives regex: {merged}"
        );
    }

    #[test]
    fn collect_dotted_paths_emits_array_index_forms() {
        // White-box check that the path enumeration produces the
        // `name[idx]` shapes the new bracket syntax addresses, so
        // regex specs can target them.
        let doc: DocumentMut = "\
[[hooks.post_create]]
cmd = \"a\"

[[hooks.post_create]]
cmd = \"b\"
"
        .parse()
        .unwrap();
        let mut paths = Vec::new();
        collect_dotted_paths(doc.as_item(), "", &mut paths);
        assert!(
            paths.iter().any(|p| p == "hooks.post_create"),
            "parent AoT path present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "hooks.post_create[0]"),
            "element 0 path present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "hooks.post_create[1]"),
            "element 1 path present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "hooks.post_create[0].cmd"),
            "inside-element path present: {paths:?}"
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
