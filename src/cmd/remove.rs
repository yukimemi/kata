//! `kata remove <template> [--at <dir>]`
//!
//! Drop a template entry from `applied.toml.templates`. Doesn't
//! delete the files the template wrote — the project may have
//! taken ownership of them since. A future `--clean` flag (Phase
//! 4) will offer to walk the file state and remove what was kata-
//! managed, with confirmation.

use camino::Utf8PathBuf;

use crate::applied::AppliedState;
use crate::error::{Error, Result};

use super::resolve_pj_root;

pub async fn run(template_name: String, at: Option<Utf8PathBuf>, no_color: bool) -> Result<()> {
    let _ = no_color;
    let cwd = resolve_pj_root(at)?;
    let pj_root = crate::paths::find_pj_root(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no .kata/applied.toml found at or above {cwd}; run `kata init` first"
        ))
    })?;

    let mut applied = AppliedState::load(&pj_root)?;

    // Match against full source spec OR the trailing path/name
    // segment so users can write `kata remove pj-rust` instead of
    // the full `github.com/yukimemi/pj-rust` URL. If the short name
    // is ambiguous (multiple templates share the same trailing
    // segment), refuse — silently dropping all of them is worse
    // than asking for the full source spec.
    let matches: Vec<String> = applied
        .templates
        .iter()
        .filter(|t| template_matches(&t.source, &template_name))
        .map(|t| t.source.clone())
        .collect();

    match matches.len() {
        0 => {
            return Err(Error::Config(format!(
                "template `{template_name}` is not applied to this project; nothing to remove"
            )));
        }
        1 => {}
        _ => {
            let listed = matches.join(", ");
            return Err(Error::Config(format!(
                "template `{template_name}` is ambiguous — matches: {listed}. Pass the full source spec."
            )));
        }
    }

    applied
        .templates
        .retain(|t| !template_matches(&t.source, &template_name));

    applied.save(&pj_root)?;
    println!(
        "removed `{template_name}` from {}/.kata/applied.toml",
        pj_root
    );
    println!(
        "(files written by the template stay in place — Phase 4 `--clean` will offer a delete pass)"
    );
    Ok(())
}

/// True when `source` (full spec from applied.toml) matches the
/// user's query — either as a literal full match or as the last
/// `/`-separated segment.
fn template_matches(source: &str, query: &str) -> bool {
    if source == query {
        return true;
    }
    source
        .rsplit('/')
        .next()
        .map(|s| s == query)
        .unwrap_or(false)
}

/// Same matching rule, exposed for `kata update`'s template
/// filter. (`pub(crate)` so it stays visible to the cmd layer
/// without leaking out of the crate.)
pub(crate) fn template_matches_pub(source: &str, query: &str) -> bool {
    template_matches(source, query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_source() {
        assert!(template_matches("github.com/x/y", "github.com/x/y"));
    }

    #[test]
    fn matches_trailing_segment() {
        assert!(template_matches("github.com/yukimemi/pj-rust", "pj-rust"));
        assert!(template_matches("./local/pj-base", "pj-base"));
    }

    #[test]
    fn does_not_match_unrelated() {
        assert!(!template_matches("github.com/yukimemi/pj-rust", "pj-base"));
    }
}
