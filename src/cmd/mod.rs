//! One file per subcommand. The dispatch table itself lives in
//! `cli.rs` (calling `cmd::<name>::run`).

pub mod apply;
pub mod doctor;
pub mod init;
pub mod list;
pub mod status;

use std::collections::BTreeMap;
use std::env;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{Error, Result};
use crate::render::parse_cli_var;

/// Resolve `--at <dir>` to an absolute path, defaulting to the
/// current working directory.
pub(crate) fn resolve_pj_root(at: Option<Utf8PathBuf>) -> Result<Utf8PathBuf> {
    let raw = match at {
        Some(p) => p,
        None => Utf8PathBuf::from_path_buf(
            env::current_dir()
                .map_err(|e| Error::io_at(env::current_dir().ok().unwrap_or_default(), e))?,
        )
        .map_err(|p| Error::Config(format!("cwd is not valid UTF-8: {}", p.display())))?,
    };
    if raw.is_absolute() {
        return Ok(raw);
    }
    let cwd = env::current_dir().map_err(|e| Error::io_at(Utf8PathBuf::new().as_std_path(), e))?;
    let abs = Utf8PathBuf::from_path_buf(cwd.join(raw.as_std_path()))
        .map_err(|p| Error::Config(format!("path is not valid UTF-8: {}", p.display())))?;
    Ok(abs)
}

/// Parse `--var name=val` into a typed table. Errors out on the first
/// invalid entry.
pub(crate) fn parse_cli_vars(items: Vec<String>) -> Result<BTreeMap<String, toml::Value>> {
    let mut out = BTreeMap::new();
    for it in items {
        let (k, v) = parse_cli_var(&it)?;
        out.insert(k, v);
    }
    Ok(out)
}

/// Make `<root>/.kata/` if missing (so `applied.toml` writes succeed
/// later). Idempotent.
pub(crate) fn ensure_state_dir(root: &Utf8Path) -> Result<()> {
    let dir = root.join(crate::paths::PJ_STATE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| Error::io_at(dir.as_std_path(), e))?;
    Ok(())
}

pub mod doctor_helpers {
    use std::process::Command;

    /// True if `cmd --version` (or just `cmd` for `which` cases) runs
    /// successfully. Used by `kata doctor` to detect tooling.
    pub fn detect(cmd: &str, args: &[&str]) -> bool {
        Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
