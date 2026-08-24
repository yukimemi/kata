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
//! file has no `name` entry at all, kata creates it at index 0
//! only. From there an index exactly one past the end appends, so
//! a layer owning `[0]`, `[1]`, `[2]` grows the array in order on
//! top of a lower layer that shipped only `[0]`. A gap is still
//! refused — `idx > len` is a silent no-op rather than padding
//! empty tables to reach it. That makes the order of `paths`
//! load-bearing: list the indices ascending, since `[2]` applied
//! before `[1]` lands on the gap rule and is dropped.
//!
//! **Inline array indexing (#111)**: the same `name[idx]` form
//! also addresses elements of an *inline* array
//! (`Item::Value(Value::Array(_))`) — `tags = ["a", "b"]`,
//! `dependencies = ["fmt", "clippy"]`, and so on. Same rules as the
//! AoT case: bootstrap at `idx == 0` when the entry is missing,
//! append at `idx == len`, refuse a gap at `idx > len`, and refuse
//! to clobber a non-array slot. The setter dispatches on the
//! incoming `Item` variant — `Item::Table` → AoT; `Item::Value` →
//! inline array — so a shape mismatch (existing is AoT but
//! incoming is inline, or vice versa) naturally bails out without
//! restructuring the consumer's file.
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
use toml_edit::{Array, ArrayOfTables, Decor, DocumentMut, Item, Table, Value};

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
                new_body: Some(new_body),
            }),
            Some(cur) if cur == new_body => Ok(ActionPlan {
                kind: PlanKind::Unchanged,
                diff: None,
                new_body: Some(new_body),
            }),
            Some(cur) => Ok(ActionPlan {
                kind: PlanKind::Update,
                diff: Some(unified_diff(cur, &new_body, ctx.dst_abs.as_str())),
                new_body: Some(new_body),
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
/// already equivalent at the path) skip the value assignment
/// entirely — see the kata#34 comment below on why that matters for
/// interleaved consumer keys — but still carry a missing key
/// comment (`set_leaf_key_decor`), so a renovate-style pin is never
/// left behind the first time the template adds it.
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
    let Some(value) = item_at_path(incoming_doc.as_item(), &segments) else {
        return Ok(()); // path absent in incoming → leave existing untouched
    };
    let incoming_decor = incoming_leaf_decor(incoming_doc.as_item(), &segments);

    // If the existing file already has the same value at this path,
    // skip the value assignment entirely — toml_edit's emit after a
    // replace can shuffle the entry relative to interleaved consumer
    // keys even for an identical value (kata#34). A missing key
    // comment is still a real diff though, so write it separately.
    let already_matches = item_at_path(existing_doc.as_item(), &segments)
        .as_ref()
        .is_some_and(|cur| items_equivalent(cur, &value));
    if already_matches {
        set_leaf_key_decor(existing_doc, &segments, &incoming_decor);
    } else {
        set_at_path(existing_doc, &segments, value, incoming_decor);
    }
    Ok(())
}

/// Copy the incoming key's comment decor onto the existing leaf key
/// when the values already match. Guards on the consumer's own
/// comment: if the existing key already has a `# ...` prefix it is
/// left alone.
fn set_leaf_key_decor(doc: &mut DocumentMut, path: &[PathSeg], decor: &Option<Decor>) {
    let Some(decor) = decor else {
        return;
    };
    let Some(PathSeg::Key(k)) = path.last() else {
        return;
    };

    // Walk down to the PARENT table of the leaf key WITH MUTABLE
    // borrows of the live document (item_at_path returns clones, so
    // writing a clone would mutate nothing). `path[..len-1]` are the
    // intermediate Key segments; only `Table` can continue, exactly
    // like item_at_table_path.
    let keys: Vec<&str> = path[..path.len() - 1]
        .iter()
        .filter_map(|s| match s {
            PathSeg::Key(k) => Some(k.as_str()),
            PathSeg::KeyIndex(..) => None,
        })
        .collect();
    let Some(parent) = table_at_path_mut(doc.as_table_mut(), &keys) else {
        return;
    };
    let Some(mut km) = parent.get_key_value_mut(k.as_str()).map(|(km, _)| km) else {
        return;
    };
    let has_own = km
        .leaf_decor()
        .prefix()
        .and_then(|p| p.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    if !has_own {
        *km.leaf_decor_mut() = decor.clone();
    }
}

/// Descend a run of Key segments through mutable `Table`s, returning
/// the parent of the leaf key. Returns None if a segment is missing or
/// resolves to a non-table (refuse-to-clobber, same as set_in_table).
fn table_at_path_mut<'a>(table: &'a mut Table, keys: &[&str]) -> Option<&'a mut Table> {
    let mut cur = table;
    for k in keys {
        cur = cur.get_mut(k).and_then(|item| item.as_table_mut())?;
    }
    Some(cur)
}

/// The comment decor (a `# ...` line that precedes the path's leaf
/// key in the incoming template body), when that key carries one.
///
/// merge-toml copies a value `Item`, and a value carries no key decor,
/// so the renovate-style `# datasource=...` pin above an action pin is
/// otherwise orphaned by every value write to the same key. Return it
/// when the incoming key has a real comment prefix — plain white-space
/// is the bare ` = ` we reproduce ourselves and renders as no comment.
fn incoming_leaf_decor(item: &Item, path: &[PathSeg]) -> Option<Decor> {
    let Some(PathSeg::Key(leaf_key)) = path.last() else {
        return None; // array-indexed leaf: no key comment to carry
    };

    // Descend to the PARENT of the leaf key, not the leaf value, and
    // read the key's decor from the table that owns it. `item_at_path`
    // on the prefix walks Key / KeyIndex the same way the writer does,
    // so the two stay in step.
    let parent = if path.len() > 1 {
        item_at_path(item, &path[..path.len() - 1])?
    } else {
        item.clone()
    };
    let Item::Table(table) = parent else {
        return None;
    };
    let decor = table
        .get_key_value(leaf_key.as_str())
        .map(|(k, _)| k.leaf_decor().clone())?;
    if decor
        .prefix()
        .and_then(|p| p.as_str())
        .is_some_and(|s| !s.trim().is_empty())
    {
        Some(decor)
    } else {
        None
    }
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
        // Inline arrays (#111): emit per-element paths so regex
        // specs can target them. Elements are scalars / inline
        // values, so don't recurse — `parse_segments` rejects
        // chained `[N][M]` and there's no Key step into a
        // non-table.
        Item::Value(Value::Array(arr)) => {
            for (idx, _elem) in arr.iter().enumerate() {
                let path = format!("{prefix}[{idx}]");
                out.push(path);
            }
        }
        // Inline tables, scalars, datetimes — no further walk.
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
            let entry = table.get(k)?;
            // Try array-of-tables first — that's the kata#107
            // shape. Inline arrays (#111) fall through to the
            // `as_array` branch below.
            if let Some(aot) = entry.as_array_of_tables() {
                let elem = aot.get(*i)?;
                if rest.is_empty() {
                    return Some(Item::Table(elem.clone()));
                }
                return item_at_table_path(elem, rest);
            }
            if let Some(arr) = entry.as_array() {
                let v = arr.get(*i)?;
                if rest.is_empty() {
                    return Some(Item::Value(v.clone()));
                }
                // Inline-array elements are scalars / inline values
                // / inline tables — none of them continue a path
                // walk under kata's current scope. Bail out instead
                // of silently producing partial results.
                return None;
            }
            None
        }
    }
}

/// Set the value at a path, creating intermediate **missing**
/// `Table`s and bootstrap `ArrayOfTables` (index 0 only) as needed.
/// An index one past the end of an existing array appends to it.
/// Refuses to clobber slots that already hold a wrong-shape item:
/// a `Key` step against an existing non-table, a `KeyIndex` step
/// against an existing non-array-of-tables, or a `KeyIndex` whose
/// index would leave a gap all silently no-op rather than
/// rewriting unrelated structure.
fn set_at_path(doc: &mut DocumentMut, path: &[PathSeg], value: Item, key_decor: Option<Decor>) {
    set_in_table(doc.as_table_mut(), path, value, key_decor);
}

fn set_in_table(table: &mut Table, path: &[PathSeg], value: Item, key_decor: Option<Decor>) {
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
                let existing = table.get_mut(k);
                if let Some(existing) = existing {
                    *existing = value;
                } else {
                    table.insert(k, value);
                }
                // The value item never carries the key's decor, so a
                // comment above the key (the `# renovate:` pin on a
                // vars.toml action, say) is orphaned by any write to
                // the same key. Restore the template's key decor when
                // the key has no comment of its own to keep.
                if let Some(decor) = key_decor {
                    if let Some(mut km) = table.key_mut(k) {
                        let has_own = km
                            .leaf_decor()
                            .prefix()
                            .and_then(|p| p.as_str())
                            .is_some_and(|s| !s.trim().is_empty());
                        if !has_own {
                            *km.leaf_decor_mut() = decor;
                        }
                    }
                }
            } else {
                let entry = table.entry(k).or_insert_with(|| Item::Table(Table::new()));
                let Some(next) = entry.as_table_mut() else {
                    return; // existing non-table intermediate — refuse to clobber
                };
                set_in_table(next, rest, value, key_decor);
            }
        }
        PathSeg::KeyIndex(k, i) => {
            if is_leaf {
                // Dispatch on the incoming `Item` variant so the
                // setter picks AoT (#107) or inline-array (#111)
                // based on what the template authored. A shape
                // mismatch with the consumer's existing file
                // naturally bails out inside the helpers
                // (`ensure_aot_element` rejects non-AoT slots,
                // `ensure_array_element` rejects non-Array slots).
                match value {
                    Item::Table(value_table) => {
                        let Some(elem) = ensure_aot_element(table, k, *i) else {
                            return;
                        };
                        *elem = value_table;
                    }
                    Item::Value(value_v) => {
                        let Some(elem) = ensure_array_element(table, k, *i) else {
                            return;
                        };
                        *elem = value_v;
                    }
                    // `Item::ArrayOfTables` and `Item::None` at the
                    // leaf don't have a sensible destination here.
                    _ => {}
                }
            } else {
                // Intermediate KeyIndex — only AoT can continue the
                // walk. Inline-array elements don't carry sub-paths
                // (their elements are scalars / inline tables which
                // terminate the walk in `item_at_table_path`).
                let Some(elem) = ensure_aot_element(table, k, *i) else {
                    return;
                };
                set_in_table(elem, rest, value, key_decor);
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
/// - key present, `idx < len` → return element `idx`
/// - key present, `idx == len` → push one fresh `Table` and
///   return it (this also covers an already-emptied array)
/// - key present, `idx > len` → no-op (padding a gap would mean
///   inventing the tables in between)
fn ensure_aot_element<'a>(table: &'a mut Table, key: &str, idx: usize) -> Option<&'a mut Table> {
    match table.entry(key) {
        toml_edit::Entry::Vacant(slot) => {
            if idx != 0 {
                return None;
            }
            let mut aot = ArrayOfTables::new();
            aot.push(Table::new());
            slot.insert(Item::ArrayOfTables(aot))
                .as_array_of_tables_mut()?
                .get_mut(0)
        }
        toml_edit::Entry::Occupied(slot) => {
            // `into_mut` returns `&'a mut Item` with the table's
            // lifetime; `get_mut` would borrow from the entry and
            // can't escape this arm.
            let aot = slot.into_mut().as_array_of_tables_mut()?;
            // Exactly one past the end appends. Layered templates
            // grow the same array in order — pj-base owning
            // `hooks.post_create[0]` and the layer above it owning
            // `[0]`, `[1]`, `[2]` — and refusing to extend by one
            // would silently drop every element past the first,
            // leaving the consumer with a half-applied chain and
            // nothing said about it. A gap is still refused: at
            // `idx > len` there is no element to write without
            // inventing the ones in between.
            if idx > aot.len() {
                return None;
            }
            if idx == aot.len() {
                aot.push(Table::new());
            }
            aot.get_mut(idx)
        }
    }
}

/// Inline-array counterpart of `ensure_aot_element` for #111.
/// Same conservative rules but on `Item::Value(Value::Array(_))`:
///
/// - key missing AND `idx != 0` → no-op
/// - key missing AND `idx == 0` → create `Value::Array(vec![placeholder])`
///   and return a borrow to slot 0 (caller overwrites the placeholder)
/// - key present, not an inline array → no-op (refuse-to-clobber;
///   covers the shape mismatch where existing is `ArrayOfTables`
///   but incoming is inline)
/// - key present, `idx < len` → return slot `idx`
/// - key present, `idx == len` → push a placeholder and return a
///   borrow to it (this also covers an already-emptied array)
/// - key present, `idx > len` → no-op
///
/// The placeholder (`Value::from(0i64)`) is overwritten by the
/// caller before `.to_string()` runs, so its choice doesn't show
/// up in the output. An integer is just a stable default.
fn ensure_array_element<'a>(table: &'a mut Table, key: &str, idx: usize) -> Option<&'a mut Value> {
    match table.entry(key) {
        toml_edit::Entry::Vacant(slot) => {
            if idx != 0 {
                return None;
            }
            let mut arr = Array::new();
            arr.push(Value::from(0i64));
            slot.insert(Item::Value(Value::Array(arr)))
                .as_array_mut()?
                .get_mut(0)
        }
        toml_edit::Entry::Occupied(slot) => {
            let arr = slot.into_mut().as_array_mut()?;
            // One past the end appends, a gap does not — the same
            // rule `ensure_aot_element` follows, for the same
            // layered-template reason.
            if idx > arr.len() {
                return None;
            }
            if idx == arr.len() {
                arr.push(Value::from(0i64));
            }
            arr.get_mut(idx)
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
    fn merge_replaces_only_index_zero_of_inline_array() {
        // yukimemi/kata#111: cargo-make-style `dependencies = [...]`
        // (inline array of strings). Template owns idx 0; consumer
        // appends their own extra step at idx 1+.
        let existing = "\
[tasks.on-add]
dependencies = [\"cargo make fmt\", \"bun install\"]
";
        let incoming = "\
[tasks.on-add]
dependencies = [\"cargo make fmt-v2\"]
";
        let merged = merge(Some(existing), incoming, &["tasks.on-add.dependencies[0]"]);
        assert!(
            merged.contains("cargo make fmt-v2"),
            "element 0 must be updated: {merged}"
        );
        assert!(
            merged.contains("bun install"),
            "consumer's element 1 must survive: {merged}"
        );
    }

    #[test]
    fn merge_bootstraps_inline_array_when_missing() {
        let existing = "[tasks.on-add]\n";
        let incoming = "\
[tasks.on-add]
dependencies = [\"cargo make fmt\"]
";
        let merged = merge(Some(existing), incoming, &["tasks.on-add.dependencies[0]"]);
        assert!(
            merged.contains("dependencies = [\"cargo make fmt\"]")
                || merged.contains("dependencies = [\"cargo make fmt\",]"),
            "missing inline array must be bootstrapped: {merged}"
        );
    }

    #[test]
    fn merge_refuses_to_clobber_non_array_at_inline_index_path() {
        // Existing has `tags` as a scalar (wrong shape). Path
        // `tags[0]` must not clobber the scalar.
        let existing = "tags = \"not-an-array\"\n";
        let incoming = "tags = [\"first\"]\n";
        let merged = merge(Some(existing), incoming, &["tags[0]"]);
        assert!(
            merged.contains("tags = \"not-an-array\""),
            "non-array intermediate must NOT be clobbered: {merged}"
        );
    }

    #[test]
    fn merge_can_replace_string_element_of_inline_array() {
        // The simple `tags[0]` case — array of strings, replace
        // element 0 only.
        let existing = "tags = [\"old\", \"keep\"]\n";
        let incoming = "tags = [\"new\"]\n";
        let merged = merge(Some(existing), incoming, &["tags[0]"]);
        assert!(merged.contains("\"new\""), "idx 0 updated: {merged}");
        assert!(merged.contains("\"keep\""), "idx 1 preserved: {merged}");
    }

    #[test]
    fn merge_inline_array_index_is_idempotent() {
        let existing = "\
[tasks.on-add]
dependencies = [\"cargo make fmt\", \"bun install\"]
";
        let incoming = "\
[tasks.on-add]
dependencies = [\"cargo make fmt\"]
";
        let first = merge(Some(existing), incoming, &["tasks.on-add.dependencies[0]"]);
        let second = merge(Some(&first), incoming, &["tasks.on-add.dependencies[0]"]);
        assert_eq!(first, second, "inline-array index merge must be idempotent");
    }

    #[test]
    fn merge_refuses_when_existing_is_aot_but_incoming_is_inline_array() {
        // Shape mismatch: existing has `[[deps]]` (AoT) while the
        // template ships `deps = [...]` (inline). Refuse to
        // restructure the consumer's data.
        let existing = "\
[[deps]]
name = \"a\"
";
        let incoming = "deps = [\"new\"]\n";
        let merged = merge(Some(existing), incoming, &["deps[0]"]);
        assert!(
            merged.contains("[[deps]]") && merged.contains("name = \"a\""),
            "AoT existing must survive when incoming is inline array: {merged}"
        );
    }

    #[test]
    fn regex_can_target_specific_inline_array_element() {
        let existing = "deps = [\"old\", \"consumer\"]\n";
        let incoming = "deps = [\"new\"]\n";
        let merged = merge(Some(existing), incoming, &[r"//^deps\[0\]$//"]);
        assert!(merged.contains("\"new\""), "regex hit idx 0: {merged}");
        assert!(
            merged.contains("\"consumer\""),
            "regex left idx 1 alone: {merged}"
        );
    }

    #[test]
    fn collect_dotted_paths_emits_inline_array_index_forms() {
        let doc: DocumentMut = "\
[tasks.on-add]
dependencies = [\"a\", \"b\"]
"
        .parse()
        .unwrap();
        let mut paths = Vec::new();
        collect_dotted_paths(doc.as_item(), "", &mut paths);
        assert!(
            paths.iter().any(|p| p == "tasks.on-add.dependencies"),
            "parent inline-array path present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "tasks.on-add.dependencies[0]"),
            "element 0 path present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "tasks.on-add.dependencies[1]"),
            "element 1 path present: {paths:?}"
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

    #[test]
    fn merge_appends_index_one_past_the_end() {
        // Existing has 1 element; the path asks for index 1,
        // exactly one past the end. That appends: a layer owning
        // `[1]` on top of a layer that shipped only `[0]` has to be
        // able to grow the array.
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
        assert!(
            merged.contains("cmd = \"keep\""),
            "element 0 is not in `paths` and must survive: {merged}"
        );
        assert!(
            merged.contains("cmd = \"second\""),
            "one past the end must append: {merged}"
        );
        assert!(
            !merged.contains("cmd = \"first\""),
            "element 0 was not requested and must not be copied: {merged}"
        );
    }

    #[test]
    fn merge_skips_index_that_would_leave_a_gap() {
        // Existing has 1 element; the path asks for index 2.
        // Reaching it would mean inventing element 1, so this stays
        // the no-op it always was.
        let existing = "\
[[hooks.post_create]]
cmd = \"keep\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"first\"

[[hooks.post_create]]
cmd = \"second\"

[[hooks.post_create]]
cmd = \"third\"
";
        let merged = merge(Some(existing), incoming, &["hooks.post_create[2]"]);
        assert!(merged.contains("cmd = \"keep\""));
        assert!(
            !merged.contains("cmd = \"third\""),
            "must not pad to reach a gapped index: {merged}"
        );
    }

    #[test]
    fn merge_grows_a_one_element_array_through_the_whole_chain() {
        // The layered case the rule exists for: the lower layer
        // shipped one hook, the layer above owns three. Applying
        // `[0]`, `[1]`, `[2]` in one pass has to land all three.
        let existing = "\
# consumer comment
[ui]
show_pr = true

[[hooks.post_create]]
cmd = \"from-base\"
";
        let incoming = "\
[[hooks.post_create]]
cmd = \"first\"

[[hooks.post_create]]
cmd = \"second\"

[[hooks.post_create]]
cmd = \"third\"
";
        let merged = merge(
            Some(existing),
            incoming,
            &[
                "hooks.post_create[0]",
                "hooks.post_create[1]",
                "hooks.post_create[2]",
            ],
        );
        for cmd in ["first", "second", "third"] {
            assert!(
                merged.contains(&format!("cmd = \"{cmd}\"")),
                "{cmd} must be applied: {merged}"
            );
        }
        assert!(
            !merged.contains("from-base"),
            "element 0 is owned by the template and must be replaced: {merged}"
        );
        assert!(
            merged.contains("show_pr = true") && merged.contains("# consumer comment"),
            "keys and comments outside `paths` must survive: {merged}"
        );
    }

    #[test]
    fn merge_appends_one_past_the_end_of_an_inline_array() {
        let existing = "deps = [\"keep\"]\n";
        let incoming = "deps = [\"first\", \"second\"]\n";
        let merged = merge(Some(existing), incoming, &["deps[1]"]);
        assert!(merged.contains("\"keep\""), "slot 0 preserved: {merged}");
        assert!(
            merged.contains("\"second\""),
            "one past the end must append: {merged}"
        );
    }

    #[test]
    fn merge_skips_gapped_index_on_shorter_inline_array() {
        let existing = "deps = [\"keep\"]\n";
        let incoming = "deps = [\"first\", \"second\", \"third\"]\n";
        let merged = merge(Some(existing), incoming, &["deps[2]"]);
        assert!(merged.contains("\"keep\""));
        assert!(
            !merged.contains("\"third\""),
            "must not pad to reach a gapped index: {merged}"
        );
    }
    // Value already current but the key carries no comment yet — the
    // value write is skipped (kata#34), but the template's comment
    // must still be adopted so a renovate pin propagates onto a
    // pre-seeded consumer.
    #[test]
    fn merge_adopts_key_comment_when_value_already_matches() {
        let existing = "[actions]\nfoo = \"v2\"\nbar = \"keep\"\n";
        let incoming = "\
[actions]
# renovate: datasource=github-tags depName=pj/action
foo = \"v2\"
";
        let merged = merge(Some(existing), incoming, &["actions.foo"]);
        assert!(
            merged.contains("# renovate"),
            "comment adopted on equal-value merge: {merged}"
        );
        assert!(
            merged.contains("bar = \"keep\""),
            "consumer key kept: {merged}"
        );
    }

    #[test]
    fn merge_keeps_consumer_comment_when_value_matches() {
        let existing = "[actions]\n# my pin\nfoo = \"v2\"\n";
        let incoming = "\
[actions]
# renovate: datasource=github-tags depName=pj/action
foo = \"v3\"
";
        let merged = merge(Some(existing), incoming, &["actions.foo"]);
        assert!(
            merged.contains("# my pin"),
            "consumer comment kept: {merged}"
        );
        assert!(
            !merged.contains("depName=pj/action"),
            "upstream comment must not replace the consumer's: {merged}"
        );
    }
}
