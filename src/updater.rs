//! Background auto-update + the `kata self-update` subcommand, built
//! on `kaishin`.
//!
//! kata is already async (`#[tokio::main]`), so this calls kaishin's
//! async API directly rather than going through a blocking-runtime
//! facade.
//!
//! Default behaviour is **opt-out silent install** (`AutoUpdateMode::
//! Install`): at the start of each command kata spawns a throttled,
//! lock-serialised background check that downloads and swaps the
//! binary when a newer release exists. The running process keeps the
//! old binary; the new version applies on the next launch. Exactly
//! one stderr line is printed, and only when an install actually
//! happened. `notify` mode prints a banner instead of installing;
//! `off` does nothing. The `KATA_NO_AUTOUPDATE` env var overrides the
//! config to `off`.
//!
//! All network / lock failures stay silent (resilience), and
//! development builds are never updated (kaishin refuses).

use std::time::Duration;

use crate::config::{AutoUpdateMode, GlobalConfig};
use crate::error::{Error, Result};

/// GitHub owner for the release repo.
const OWNER: &str = "yukimemi";
/// Binary / repo / crate name (all `kata`).
const BIN: &str = env!("CARGO_PKG_NAME");
/// Compiled-in crate version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Max time `finalize_auto_update_check` waits for the background
/// install to finish before giving up silently, so fast commands
/// never hang on a slow download.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the shared kaishin options for kata.
fn kaishin_opts() -> kaishin::KaishinOptions {
    kaishin::KaishinOptions::new(OWNER, BIN, BIN, VERSION)
}

/// State file for the throttle / cached-latest. Lives under kata's
/// own cache dir (honouring `$KATA_HOME`) so it doesn't clobber
/// kaishin's default data-dir location and stays test-isolated.
fn state_path() -> Option<std::path::PathBuf> {
    crate::paths::template_cache_dir()
        .ok()
        .and_then(|c| c.parent().map(|p| p.to_path_buf()))
        .map(|cache| cache.join("last_update_check.json").into_std_path_buf())
}

/// Pure truthiness of the kill-switch value: disabled when the var is
/// present, non-empty (after trim), and not `"0"` / `"false"`
/// (case-insensitive).
///
/// Split out from [`auto_update_disabled_by_env`] so the decision logic
/// can be unit-tested **by value** without mutating the global process
/// environment (which would race under the default parallel test runner).
fn env_value_disables(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim();
            !v.is_empty() && !v.eq_ignore_ascii_case("0") && !v.eq_ignore_ascii_case("false")
        }
        None => false,
    }
}

/// True when the `KATA_NO_AUTOUPDATE` env kill-switch is engaged:
/// present, non-empty, and not `0` / `false` (case-insensitive).
/// Takes precedence over the config.
pub fn auto_update_disabled_by_env() -> bool {
    env_value_disables(std::env::var("KATA_NO_AUTOUPDATE").ok().as_deref())
}

/// Resolve the effective mode: env kill-switch first (→ `Off`), then
/// the global config's `defaults.update_mode()`, then the default
/// (`Install`) if the config couldn't be loaded. Takes the
/// already-loaded config by reference so startup reads it only once.
fn resolve_mode(config: Option<&GlobalConfig>) -> AutoUpdateMode {
    if auto_update_disabled_by_env() {
        return AutoUpdateMode::Off;
    }
    config.map(|c| c.defaults.update_mode()).unwrap_or_default()
}

/// Build a kaishin `Checker` honouring the configured interval and
/// kata's cache-dir state path. Returns `None` if no usable state
/// path can be resolved (silent skip). Takes the already-loaded
/// config by reference so startup reads it only once.
fn build_checker(config: Option<&GlobalConfig>) -> Option<kaishin::Checker> {
    let interval = config
        .and_then(|c| c.defaults.update_check_interval.as_deref())
        .and_then(|s| kaishin::parse_interval(s).ok())
        .unwrap_or_else(kaishin::default_interval);

    let mut checker = kaishin::Checker::new(BIN, kaishin_opts()).interval(interval);
    if let Some(p) = state_path() {
        checker = checker.state_path(p);
    }
    Some(checker)
}

/// In-flight background auto-update, consumed by
/// [`finalize_auto_update_check`].
pub enum AutoUpdateHandle {
    /// `notify` mode: a background task that fetches the latest
    /// release and, if newer, returns it for a banner.
    Notify {
        checker: kaishin::Checker,
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>, anyhow::Error>>,
    },
    /// `install` mode: a background task that silently self-installs
    /// and returns the release iff it actually installed.
    Install {
        handle: tokio::task::JoinHandle<Result<Option<kaishin::LatestRelease>, anyhow::Error>>,
    },
}

/// Spawn the background auto-update check appropriate to the resolved
/// mode. Returns `None` for `off`, on config-load failure, or when no
/// state path is available — all silent. The returned handle (if any)
/// must be passed to [`finalize_auto_update_check`] at the end of the
/// command.
pub async fn maybe_spawn_auto_update_check() -> Option<AutoUpdateHandle> {
    // Load the global config once and share it across the resolver and
    // the checker builder to avoid a redundant synchronous disk read
    // at the start of every command.
    let config = GlobalConfig::load().ok();
    match resolve_mode(config.as_ref()) {
        AutoUpdateMode::Off => None,
        AutoUpdateMode::Notify => {
            let checker = build_checker(config.as_ref())?;
            let worker = checker.clone();
            let handle = tokio::spawn(async move { worker.check_and_save().await });
            Some(AutoUpdateHandle::Notify { checker, handle })
        }
        AutoUpdateMode::Install => {
            let checker = build_checker(config.as_ref())?;
            // `auto_update` is self-throttled, lock-serialised, and
            // returns Ok(Some(rel)) only when it actually installed.
            let handle = tokio::spawn(async move { checker.auto_update().await });
            Some(AutoUpdateHandle::Install { handle })
        }
    }
}

/// Consume the background handle. For `notify`, print the banner when
/// a newer release exists. For `install`, bounded-wait the worker and
/// print exactly one line iff an install happened. Timeouts, errors,
/// and "nothing to do" all print nothing (silent resilience).
pub async fn finalize_auto_update_check(handle: AutoUpdateHandle) {
    match handle {
        AutoUpdateHandle::Notify { checker, handle } => {
            let res = tokio::time::timeout(FINALIZE_TIMEOUT, handle).await;
            if let Ok(Ok(Ok(Some(latest)))) = res {
                eprintln!("\n{}", checker.format_banner(&latest));
            }
        }
        AutoUpdateHandle::Install { handle } => {
            let res = tokio::time::timeout(FINALIZE_TIMEOUT, handle).await;
            if let Ok(Ok(Ok(Some(rel)))) = res {
                let version = rel.tag_name.trim_start_matches('v');
                eprintln!(
                    "\u{2713} {BIN} {version} installed in the background — restart to apply."
                );
            }
        }
    }
}

/// `kata self-update [--yes] [--check]`. Drives kaishin's interactive
/// self-update flow, mapping its `anyhow::Error` into kata's `Error`.
pub async fn run_self_update(yes: bool, check_only: bool, non_interactive: bool) -> Result<()> {
    let upd = kaishin::UpdateOptions::new()
        .yes(yes)
        .check_only(check_only)
        .non_interactive(non_interactive);
    kaishin::run_self_update(&kaishin_opts(), upd)
        .await
        .map_err(Error::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Kill-switch decision logic (pure, tested by value) -----------------
    //
    // The truthiness rule is exercised through `env_value_disables`, which
    // takes the value as an argument. No process-env mutation, so these run
    // safely under the default parallel test runner (no data race).

    #[test]
    fn env_kill_switch_unset_is_enabled() {
        assert!(!env_value_disables(None), "absent → not disabled");
    }

    #[test]
    fn env_kill_switch_truthy_disables() {
        for v in ["1", "true", "TRUE", "yes", "on", " 1 "] {
            assert!(env_value_disables(Some(v)), "{v:?} → disabled");
        }
    }

    #[test]
    fn env_kill_switch_falsey_stays_enabled() {
        for v in ["", "  ", "0", "false", "False", "FALSE", " 0 ", " false "] {
            assert!(!env_value_disables(Some(v)), "{v:?} → not disabled");
        }
    }

    // --- Env-touching smoke test (serialised) -------------------------------
    //
    // The truthiness above is what's tested broadly. Here we only confirm the
    // env-reading wrapper wires through to the pure helper. It mutates the
    // process-global env var, so it serialises through a shared mutex to avoid
    // the parallel-test env data race. The guard is never held across an
    // `.await` (this test is synchronous), so it stays clippy-clean.

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn auto_update_disabled_by_env_reads_the_var() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "KATA_NO_AUTOUPDATE";
        let prev = std::env::var_os(key);

        // SAFETY: serialised via ENV_LOCK so no other test mutates env
        // concurrently; the previous value is restored before release.
        unsafe {
            std::env::remove_var(key);
            assert!(!auto_update_disabled_by_env(), "absent → not disabled");

            std::env::set_var(key, "1");
            assert!(auto_update_disabled_by_env(), "set → disabled");

            std::env::set_var(key, "0");
            assert!(!auto_update_disabled_by_env(), "\"0\" → not disabled");

            match prev {
                Some(val) => std::env::set_var(key, val),
                None => std::env::remove_var(key),
            }
        }
    }
}
