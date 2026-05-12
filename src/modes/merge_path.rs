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
}
