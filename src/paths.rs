use camino::{Utf8Path, Utf8PathBuf};
use directories::ProjectDirs;

use crate::error::{Error, Result};

/// PJ-side state directory name (under each managed project root).
pub const PJ_STATE_DIR: &str = ".kata";
/// PJ-side state file name (under `<pj>/.kata/`).
pub const APPLIED_FILE: &str = "applied.toml";
/// Global config file name (under `<config-dir>/kata/`).
pub const GLOBAL_CONFIG_FILE: &str = "config.toml";

/// Returns the directory used for kata's global config.
///
/// Resolution order:
/// 1. `$KATA_HOME` (test-friendly override; the whole kata config tree
///    is rooted here).
/// 2. Platform default via `directories::ProjectDirs` (e.g.
///    `~/.config/kata/` on Linux, `%APPDATA%\kata\config\` on Windows).
pub fn global_config_dir() -> Result<Utf8PathBuf> {
    if let Some(home) = std::env::var_os("KATA_HOME") {
        let pb = Utf8PathBuf::from_path_buf(home.into())
            .map_err(|p| Error::Config(format!("KATA_HOME is not valid UTF-8: {}", p.display())))?;
        return Ok(pb);
    }
    let pd = ProjectDirs::from("", "yukimemi", "kata").ok_or_else(|| {
        Error::Config("could not determine platform config directory".to_string())
    })?;
    Utf8PathBuf::from_path_buf(pd.config_dir().to_path_buf())
        .map_err(|p| Error::Config(format!("config dir is not valid UTF-8: {}", p.display())))
}

/// Returns the path to the global `config.toml`.
pub fn global_config_path() -> Result<Utf8PathBuf> {
    Ok(global_config_dir()?.join(GLOBAL_CONFIG_FILE))
}

/// Returns the directory used for cached template repositories.
///
/// Resolution: `$KATA_HOME/cache/templates/` if set, otherwise the
/// platform cache dir (e.g. `~/.cache/kata/templates/`).
pub fn template_cache_dir() -> Result<Utf8PathBuf> {
    if let Some(home) = std::env::var_os("KATA_HOME") {
        let pb = Utf8PathBuf::from_path_buf(home.into())
            .map_err(|p| Error::Config(format!("KATA_HOME is not valid UTF-8: {}", p.display())))?;
        return Ok(pb.join("cache").join("templates"));
    }
    let pd = ProjectDirs::from("", "yukimemi", "kata")
        .ok_or_else(|| Error::Config("could not determine platform cache directory".to_string()))?;
    let cache = Utf8PathBuf::from_path_buf(pd.cache_dir().to_path_buf())
        .map_err(|p| Error::Config(format!("cache dir is not valid UTF-8: {}", p.display())))?;
    Ok(cache.join("templates"))
}

/// `<pj_root>/.kata/applied.toml`
pub fn applied_path(pj_root: &Utf8Path) -> Utf8PathBuf {
    pj_root.join(PJ_STATE_DIR).join(APPLIED_FILE)
}

/// Walk up from `start` looking for the nearest directory containing
/// `.kata/applied.toml`. Returns the project root (the dir containing
/// `.kata/`), not the state file itself.
pub fn find_pj_root(start: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut cur: Option<&Utf8Path> = Some(start);
    while let Some(dir) = cur {
        if dir.join(PJ_STATE_DIR).join(APPLIED_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn applied_path_joins_correctly() {
        let p = Utf8Path::new("/tmp/myproj");
        let expected = Utf8PathBuf::from("/tmp/myproj")
            .join(PJ_STATE_DIR)
            .join(APPLIED_FILE);
        assert_eq!(applied_path(p), expected);
    }

    #[test]
    fn find_pj_root_walks_up() {
        let td = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(PJ_STATE_DIR)).unwrap();
        std::fs::write(root.join(PJ_STATE_DIR).join(APPLIED_FILE), "").unwrap();

        let found = find_pj_root(&nested).expect("should find ancestor");
        assert_eq!(
            std::fs::canonicalize(found.as_std_path()).unwrap(),
            std::fs::canonicalize(root.as_std_path()).unwrap()
        );
    }

    #[test]
    fn find_pj_root_returns_none_when_missing() {
        let td = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(td.path().to_path_buf()).unwrap();
        assert!(find_pj_root(&root).is_none());
    }
}
