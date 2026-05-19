//! Path-spec parsing shared by `merge-toml` and `merge-yaml`.
//!
//! A `paths` entry is either:
//!
//! - **Literal** — a plain dotted path like `dependencies.serde`.
//!   Existing behaviour: kata extracts the value at that path from
//!   the incoming body and copies it to the existing file at the
//!   same path.
//! - **Regex** — wrapped in `//...//`, rvpm-style. kata walks every
//!   dotted path in the incoming body and copies each path that
//!   matches the regex. Useful for "sweep every `tasks.*` task
//!   without enumerating every sub-task name" (yukimemi/kata#62).
//!
//! Both forms can mix freely in the same `paths` list.

use crate::error::{Error, Result};

/// One element of a `paths = [...]` array, post-parsing.
#[derive(Debug)]
pub enum PathSpec {
    /// A literal dotted path, e.g. `tasks.test` — the historic form.
    /// Stored as the original string so callers can split it on `.`
    /// at the point where they need `&str` segments (avoiding a
    /// borrow-vs-lifetime tangle between the parser and the walker).
    Literal(String),
    /// An rvpm-style regex written as `//pattern//`. The wrapping
    /// delimiters are stripped at parse time; this is the compiled
    /// inner pattern, matched against the incoming document's
    /// dotted-path keys.
    Regex(regex::Regex),
}

/// Parse one `paths` entry into [`PathSpec`]. A string of the form
/// `//pattern//` (length >= 4, both ends `//`) is treated as a
/// regex; everything else is a literal path.
///
/// Returns an error if the regex inside `//.../` fails to compile —
/// the message embeds the original string so manifest authors can
/// spot the offending entry.
pub fn parse_path_spec(s: &str) -> Result<PathSpec> {
    if s.len() >= 4 && s.starts_with("//") && s.ends_with("//") {
        let inner = &s[2..s.len() - 2];
        let re = regex::Regex::new(inner)
            .map_err(|e| Error::Merge(format!("invalid regex in path `{s}`: {e}")))?;
        return Ok(PathSpec::Regex(re));
    }
    Ok(PathSpec::Literal(s.to_string()))
}

/// One step of a parsed literal path. `Key` is a plain table-key
/// step; `KeyIndex` is the `name[idx]` form that addresses one
/// element of an array-of-tables (yukimemi/kata#107).
///
/// Kept module-private intent: callers in `merge_toml.rs` walk
/// these segments to descend through `Item::Table` /
/// `Item::ArrayOfTables`. `merge-yaml` doesn't consume this yet —
/// its data model has analogous sequence indexing, but landing it
/// there is a follow-up to keep the #107 PR reviewable.
#[derive(Debug, PartialEq, Eq)]
pub enum PathSeg {
    /// A bare table-key step, e.g. `tasks` in `tasks.test`.
    Key(String),
    /// A `name[idx]` step addressing element `idx` of the
    /// array-of-tables at key `name`. Stored split so the walker
    /// doesn't re-parse the bracket form per descent.
    KeyIndex(String, usize),
}

/// Split a literal path string into [`PathSeg`]s. Splits on `.`
/// (same naive rule as before #107) and then recognises a trailing
/// `[N]` on each segment as an array-index step. Empty segments
/// (leading dot, trailing dot, `a..b`) and malformed brackets are
/// rejected here so manifest authors get a specific error instead
/// of a silent no-op deep in the walker.
///
/// The bracket form must terminate a dotted segment —
/// `hooks.post_create[0].cwd` works, `hooks.post_create[0]extra`
/// does not. Indices are decimal `usize`; negative or non-numeric
/// indices are an error.
pub fn parse_segments(s: &str) -> Result<Vec<PathSeg>> {
    let mut out = Vec::new();
    for raw in s.split('.') {
        if raw.is_empty() {
            return Err(Error::Merge(format!(
                "empty segment in path `{s}` (e.g. trailing dot)"
            )));
        }
        if let Some(open) = raw.find('[') {
            if !raw.ends_with(']') {
                return Err(Error::Merge(format!(
                    "malformed path segment `{raw}` in `{s}` (expected `name[N]`)"
                )));
            }
            let name = &raw[..open];
            let idx_str = &raw[open + 1..raw.len() - 1];
            if name.is_empty() {
                return Err(Error::Merge(format!(
                    "empty key before `[` in segment `{raw}` (in `{s}`)"
                )));
            }
            let idx: usize = idx_str.parse().map_err(|e| {
                Error::Merge(format!(
                    "invalid array index `{idx_str}` in segment `{raw}` (in `{s}`): {e}"
                ))
            })?;
            out.push(PathSeg::KeyIndex(name.to_string(), idx));
        } else {
            out.push(PathSeg::Key(raw.to_string()));
        }
    }
    Ok(out)
}

/// From the full list of incoming dotted paths, pick every path
/// that matches `re` AND has no ancestor (in the matched subset)
/// also matching `re`. Copying an ancestor already brings in the
/// whole subtree, so the children would be redundant traversals
/// and (for merge-toml) redundant `items_equivalent` calls. See
/// #90 review.
///
/// Ancestor detection uses dotted-prefix comparison anchored on
/// the step boundary characters `.` and `[`, so `tasks` is treated
/// as an ancestor of both `tasks.test` and `tasks[0]` but NOT of
/// `tasks-clean.foo` (`-` isn't a step boundary). Two-pass avoids
/// the quadratic blow-up of comparing every matched path against
/// every other: pass 1 collects the matches, pass 2 keeps the ones
/// whose dotted ancestors aren't in the set.
pub fn shallowest_matches(all_paths: &[String], re: &regex::Regex) -> Vec<String> {
    let mut matched: Vec<&String> = all_paths.iter().filter(|p| re.is_match(p)).collect();
    if matched.len() <= 1 {
        return matched.into_iter().cloned().collect();
    }
    // Sort by length so the shallowest ancestors come first — the
    // retain step below can then short-circuit early when an
    // ancestor is found.
    matched.sort_by_key(|p| p.len());
    let mut keep: Vec<&String> = Vec::with_capacity(matched.len());
    for p in matched {
        let has_ancestor_in_keep = keep.iter().any(|k| {
            if k.len() >= p.len() || !p.starts_with(k.as_str()) {
                return false;
            }
            let next = p.as_bytes()[k.len()];
            // `[` extends the boundary set so `tasks` covers
            // `tasks[0]` (yukimemi/kata#107). `-` and other
            // non-boundary bytes keep the sibling-prefix immunity
            // (`tasks` does NOT cover `tasks-clean.foo`).
            next == b'.' || next == b'['
        });
        if !has_ancestor_in_keep {
            keep.push(p);
        }
    }
    keep.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_path_round_trips() {
        let spec = parse_path_spec("tasks.test").unwrap();
        match spec {
            PathSpec::Literal(s) => assert_eq!(s, "tasks.test"),
            PathSpec::Regex(_) => panic!("literal should not parse as regex"),
        }
    }

    #[test]
    fn regex_path_strips_delimiters() {
        let spec = parse_path_spec(r"//^tasks\..+$//").unwrap();
        match spec {
            PathSpec::Regex(re) => {
                assert!(re.is_match("tasks.test"));
                assert!(re.is_match("tasks.lock-check"));
                assert!(!re.is_match("dependencies.serde"));
            }
            PathSpec::Literal(_) => panic!("//...// should parse as regex"),
        }
    }

    #[test]
    fn double_slash_inside_a_literal_is_not_a_regex() {
        // A bare "//" is too short to satisfy the wrapping rule
        // (len < 4), so it stays literal. Defensible because a
        // user actually meaning the regex `""` would write that
        // out explicitly; nobody writes `//` to mean "match
        // anything".
        let spec = parse_path_spec("//").unwrap();
        assert!(matches!(spec, PathSpec::Literal(s) if s == "//"));
    }

    #[test]
    fn invalid_regex_surfaces_an_error() {
        let err = parse_path_spec(r"//[unbalanced//").unwrap_err();
        assert!(matches!(err, Error::Merge(_)));
    }

    #[test]
    fn shallowest_matches_drops_descendants_when_ancestor_already_matches() {
        // Regex matches everything → keep only top-level keys.
        let paths = vec![
            "tasks".to_string(),
            "tasks.default".to_string(),
            "tasks.default.deps".to_string(),
            "tasks.check".to_string(),
            "dependencies".to_string(),
            "dependencies.serde".to_string(),
        ];
        let re = regex::Regex::new(".+").unwrap();
        let mut kept = shallowest_matches(&paths, &re);
        kept.sort();
        assert_eq!(kept, vec!["dependencies".to_string(), "tasks".to_string()],);
    }

    #[test]
    fn shallowest_matches_does_not_treat_sibling_prefixes_as_ancestors() {
        // `tasks` is NOT an ancestor of `tasks-clean.foo` because
        // the next byte after the prefix isn't `.`. Both must
        // survive.
        let paths = vec!["tasks".to_string(), "tasks-clean.foo".to_string()];
        let re = regex::Regex::new(".+").unwrap();
        let mut kept = shallowest_matches(&paths, &re);
        kept.sort();
        assert_eq!(
            kept,
            vec!["tasks".to_string(), "tasks-clean.foo".to_string()],
        );
    }

    #[test]
    fn shallowest_matches_keeps_lone_leaf_when_ancestor_not_matched() {
        // Regex matches a leaf but not its parent — the leaf is the
        // shallowest reachable match; keep it.
        let paths = vec!["tasks".to_string(), "tasks.test".to_string()];
        let re = regex::Regex::new(r"\.test$").unwrap();
        let kept = shallowest_matches(&paths, &re);
        assert_eq!(kept, vec!["tasks.test".to_string()]);
    }

    #[test]
    fn shallowest_matches_treats_open_bracket_as_step_boundary() {
        // yukimemi/kata#107: a key followed by `[N]` is an array
        // index step. `hooks.post_create` must be recognised as an
        // ancestor of `hooks.post_create[0]` so a broad regex that
        // matches both keeps only the parent (copying the parent
        // already brings every element along).
        let paths = vec![
            "hooks.post_create".to_string(),
            "hooks.post_create[0]".to_string(),
            "hooks.post_create[0].cwd".to_string(),
        ];
        let re = regex::Regex::new(".+").unwrap();
        let kept = shallowest_matches(&paths, &re);
        assert_eq!(kept, vec!["hooks.post_create".to_string()]);
    }

    #[test]
    fn parse_segments_handles_plain_dotted_path() {
        let segs = parse_segments("tasks.test").unwrap();
        assert_eq!(
            segs,
            vec![
                PathSeg::Key("tasks".to_string()),
                PathSeg::Key("test".to_string()),
            ],
        );
    }

    #[test]
    fn parse_segments_recognises_trailing_index() {
        let segs = parse_segments("hooks.post_create[0]").unwrap();
        assert_eq!(
            segs,
            vec![
                PathSeg::Key("hooks".to_string()),
                PathSeg::KeyIndex("post_create".to_string(), 0),
            ],
        );
    }

    #[test]
    fn parse_segments_supports_index_in_middle_of_path() {
        // `hooks.post_create[0].cwd` is a valid descent: array
        // index step, then a key step inside the element table.
        let segs = parse_segments("hooks.post_create[0].cwd").unwrap();
        assert_eq!(
            segs,
            vec![
                PathSeg::Key("hooks".to_string()),
                PathSeg::KeyIndex("post_create".to_string(), 0),
                PathSeg::Key("cwd".to_string()),
            ],
        );
    }

    #[test]
    fn parse_segments_rejects_empty_segment() {
        let err = parse_segments("a..b").unwrap_err();
        assert!(matches!(err, Error::Merge(_)));
    }

    #[test]
    fn parse_segments_rejects_malformed_bracket() {
        // No trailing `]`.
        assert!(matches!(
            parse_segments("hooks.post_create[0").unwrap_err(),
            Error::Merge(_),
        ));
        // Trailing junk after `]`.
        assert!(matches!(
            parse_segments("hooks.post_create[0]extra").unwrap_err(),
            Error::Merge(_),
        ));
        // Empty name before `[`.
        assert!(matches!(
            parse_segments("[0]").unwrap_err(),
            Error::Merge(_),
        ));
        // Non-numeric index.
        assert!(matches!(
            parse_segments("hooks.post_create[oops]").unwrap_err(),
            Error::Merge(_),
        ));
    }
}
