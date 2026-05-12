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

/// From the full list of incoming dotted paths, pick every path
/// that matches `re` AND has no ancestor (in the matched subset)
/// also matching `re`. Copying an ancestor already brings in the
/// whole subtree, so the children would be redundant traversals
/// and (for merge-toml) redundant `items_equivalent` calls. See
/// #90 review.
///
/// Ancestor detection uses dotted-prefix comparison anchored on
/// `.` so `tasks` doesn't accidentally swallow `tasks-clean`
/// (`tasks-clean.foo` is NOT a descendant of `tasks`). Two-pass
/// avoids the quadratic blow-up of comparing every matched path
/// against every other: pass 1 collects the matches, pass 2
/// keeps the ones whose dotted ancestors aren't in the set.
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
            k.len() < p.len() && p.as_bytes()[k.len()] == b'.' && p.starts_with(k.as_str())
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
}
