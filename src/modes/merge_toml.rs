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

use std::path::PathBuf;

use async_trait::async_trait;
use toml_edit::{DocumentMut, Item, Table};

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

    for path_str in paths {
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
            set_at_path(&mut existing_doc, &segments, value);
        }
        // path absent in incoming → leave existing untouched
    }

    Ok(existing_doc.to_string())
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
        table.insert(last, value);
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
                for path_str in &paths_owned {
                    let segments: Vec<&str> = path_str.split('.').collect();
                    if let Some(v) = item_at_path(incoming_doc.as_item(), &segments).cloned() {
                        set_at_path(&mut existing_doc, &segments, v);
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
}
