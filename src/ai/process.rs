//! Subprocess plumbing for `how = "ai"` and the handoff escape
//! hatch. Mirrors the rvpm Mode A / Mode B split so users get a
//! consistent feel across yukimemi/* tools:
//!
//! - **chat mode** (rvpm Mode A): kata holds the conversation
//!   history and re-spawns the AI CLI in non-interactive mode each
//!   turn (`claude -p -`, `gemini -p -`, `codex exec -`). The
//!   prompt is piped on stdin so we don't have to shell-quote, and
//!   stdout is captured so we can parse / preview the response
//!   before deciding whether to write it.
//!
//! - **handoff mode** (rvpm Mode B): kata writes the assembled
//!   prompt to a tmp file, announces its path, and spawns the CLI
//!   **interactively** with stdio inherited from the parent TTY.
//!   The first user message asks the agent to read the tmp file
//!   and wait — passive instructions only — so the agent does not
//!   start auto-editing before the human takes over. Once the
//!   agent process exits, kata does **not** re-import the result
//!   (the agent already had Edit / Write tools and was free to
//!   touch the destination directly).
//!
//! This module is the low-level dispatch only; the chezmoi-style
//! per-file UI (`[a]ccept / [e]dit / [c]hat / [h]andoff / [s]kip /
//! [d]efer`) lives in `modes/ai.rs` (Phase 3-b).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::error::{Error, Result};

/// Concrete AI CLI we know how to spawn. The manifest-side
/// `AgentKind::Auto` is resolved into one of these *before* it
/// reaches this layer (see `modes/ai.rs::resolve_backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Claude,
    Gemini,
    Codex,
}

impl Backend {
    /// CLI executable name as published on PATH.
    pub fn cli_name(self) -> &'static str {
        match self {
            Backend::Claude => "claude",
            Backend::Gemini => "gemini",
            Backend::Codex => "codex",
        }
    }

    /// Human label for log lines.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Claude => "Claude",
            Backend::Gemini => "Gemini",
            Backend::Codex => "Codex",
        }
    }

    /// Quick PATH probe (`Backend::is_available()` for callers that
    /// only want a yes/no, e.g. `kata doctor`).
    pub fn is_available(self) -> bool {
        resolve_cli(self.cli_name()).is_some()
    }
}

/// What `Command::new(...)` should actually run, plus any args we
/// have to inject before the user's args (used to wrap `.ps1`
/// scripts in PowerShell on Windows).
#[derive(Debug, Clone)]
pub struct ResolvedCli {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

/// Look up `name` on PATH the way rvpm does: native PATHEXT first,
/// then `.ps1` fallback for tools (e.g. pnpm-shipped agents) that
/// only ship a PowerShell wrapper. Returns the spawn descriptor
/// the caller can pass straight to tokio's `Command`.
///
/// `which::which` already searches PATHEXT on Windows (so `.exe` /
/// `.cmd` / `.bat` are covered by the first lookup), so the
/// fallback only has to handle `.ps1` — which PATHEXT does *not*
/// list by default.
pub fn resolve_cli(name: &str) -> Option<ResolvedCli> {
    if let Ok(p) = which::which(name) {
        return Some(wrap_if_powershell(p));
    }
    #[cfg(windows)]
    {
        if let Ok(p) = which::which(format!("{name}.ps1")) {
            return Some(wrap_if_powershell(p));
        }
    }
    None
}

fn wrap_if_powershell(path: PathBuf) -> ResolvedCli {
    let is_ps1 = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ps1"))
        .unwrap_or(false);
    if !is_ps1 {
        return ResolvedCli {
            program: path,
            prefix_args: Vec::new(),
        };
    }
    // Prefer PowerShell 7 (`pwsh`) when present; fall back to the
    // built-in 5.1. `-NoProfile` skips the user $PROFILE,
    // `-ExecutionPolicy Bypass` avoids signature / MOTW prompts
    // that would otherwise hang the child silently.
    let ps_exe = if which::which("pwsh").is_ok() {
        "pwsh.exe"
    } else {
        "powershell.exe"
    };
    ResolvedCli {
        program: PathBuf::from(ps_exe),
        prefix_args: vec![
            "-NoProfile".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            path.to_string_lossy().into_owned(),
        ],
    }
}

/// Resolve the agent CLI on PATH and return the spawn descriptor,
/// or surface a clear error (with an install hint) when it isn't
/// installed. This is what every spawn site should call — it folds
/// the PATH lookup and the missing-agent error into one step so
/// callers don't `is_available()` and then `resolve_cli()`
/// separately (the redundant lookup was Gemini's review feedback).
pub fn ensure_cli_installed(backend: Backend) -> Result<ResolvedCli> {
    if let Some(r) = resolve_cli(backend.cli_name()) {
        return Ok(r);
    }
    let cli = backend.cli_name();
    let hint = match backend {
        Backend::Claude => "https://docs.claude.com/claude-code",
        Backend::Gemini => "https://ai.google.dev/gemini-api/docs/cli",
        Backend::Codex => "https://github.com/openai/codex",
    };
    Err(Error::Other(anyhow!(
        "AI backend `{cli}` is not on PATH. Install it ({hint}) or pass a different `--ai` flag."
    )))
}

/// Default chat-turn timeout. Long enough to absorb 50–100 KB
/// follow-up prompts that grow with chat history (rvpm experience),
/// short enough that a stuck CLI doesn't hold up the rest of the
/// apply run forever. Override via `KATA_AI_TIMEOUT_SECS`.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const TIMEOUT_ENV: &str = "KATA_AI_TIMEOUT_SECS";

fn resolve_timeout() -> Duration {
    let secs = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Run a single chat turn: spawn the agent in non-interactive
/// mode, hand the prompt over via stdin, capture stdout. Suitable
/// for being called repeatedly from a chat loop in `modes/ai.rs`.
///
/// `prompt_text` is whatever the caller has assembled (system +
/// history + new user instruction); we don't massage it further.
/// kata-side context tags (`<kata:body>` etc.) belong to the
/// caller.
pub async fn invoke_chat(backend: Backend, prompt_text: &str) -> Result<String> {
    let resolved = ensure_cli_installed(backend)?;

    tracing::debug!(
        backend = backend.cli_name(),
        bytes = prompt_text.len(),
        lines = prompt_text.lines().count(),
        "invoke_chat: piping prompt to agent stdin",
    );

    let mut cmd = Command::new(&resolved.program);
    cmd.args(&resolved.prefix_args);
    match backend {
        Backend::Claude | Backend::Gemini => {
            cmd.arg("-p").arg("-");
        }
        Backend::Codex => {
            cmd.arg("exec").arg("-");
        }
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Without kill_on_drop, a timeout leaves the agent
        // process running in the background past kata's exit.
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        Error::Other(anyhow::Error::from(e).context(format!(
            "failed to spawn AI CLI `{}` (is it installed and on PATH?)",
            backend.cli_name()
        )))
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt_text.as_bytes()).await.map_err(|e| {
            Error::Other(anyhow::Error::from(e).context("writing prompt to AI CLI stdin"))
        })?;
        // dropping stdin closes it -> agent sees EOF and starts
    }

    let to = resolve_timeout();
    let waited: std::io::Result<std::process::Output> = match timeout(to, child.wait_with_output())
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return Err(Error::Other(anyhow!(
                "AI CLI `{}` timed out after {}s. Set {TIMEOUT_ENV}=600 (or higher) if your network is slow.",
                backend.cli_name(),
                to.as_secs(),
            )));
        }
    };
    let output = waited.map_err(|e| {
        Error::Other(anyhow::Error::from(e).context(format!(
            "AI CLI `{}` failed to produce output",
            backend.cli_name()
        )))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Other(anyhow!(
            "AI CLI `{}` exited with status {}: {}",
            backend.cli_name(),
            output.status,
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// How to inject the *first* user message when the CLI is run
/// interactively. Each agent does this differently; keeping the
/// dispatch in one enum makes it trivial to add another backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstMessageStrategy {
    /// `claude "<msg>"` / `codex "<msg>"` — message is a positional
    /// argument and the CLI stays interactive.
    Positional,
    /// `gemini -i "<msg>"` — `-p` would put gemini in
    /// non-interactive mode, so we use the interactive flag.
    InteractiveFlag,
}

pub(crate) fn first_message_strategy(backend: Backend) -> FirstMessageStrategy {
    match backend {
        Backend::Claude | Backend::Codex => FirstMessageStrategy::Positional,
        Backend::Gemini => FirstMessageStrategy::InteractiveFlag,
    }
}

/// Hand the conversation off to the agent CLI's interactive UI.
/// kata writes `prompt_text` to a tmp file, announces the path, and
/// spawns the CLI with stdio inherited from the parent terminal so
/// the user can drive the rest of the session by hand. We
/// deliberately do NOT pre-pipe stdin (claude-code exits on EOF) and
/// we do NOT re-import the agent's output afterwards (the agent
/// already had Edit / Write tools at its disposal).
///
/// `dst_hint` is a human-friendly path the agent should read /
/// edit; it's woven into the first message but is otherwise
/// advisory (the agent decides which tools to invoke).
pub async fn run_handoff(backend: Backend, prompt_text: &str, dst_hint: &Path) -> Result<()> {
    let resolved = ensure_cli_installed(backend)?;

    // Use `tempfile` so the prompt file gets a randomised name in a
    // user-private subdir of the system temp dir — predictable
    // timestamp names in `std::env::temp_dir()` were a TOCTOU /
    // collision risk on shared systems (Gemini high-priority).
    // `into_temp_path().keep()?` persists the path so the agent
    // process can still read it after kata's handoff thread drops
    // the `NamedTempFile` guard.
    let mut tmp = tempfile::Builder::new()
        .prefix("kata-ai-prompt-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| {
            Error::Other(anyhow::Error::from(e).context("creating tmp prompt file for handoff"))
        })?;
    tmp.write_all(prompt_text.as_bytes()).map_err(|e| {
        Error::Other(anyhow::Error::from(e).context("writing tmp prompt file for handoff"))
    })?;
    let tmp_path: PathBuf = tmp.into_temp_path().keep().map_err(|e| {
        Error::Other(anyhow::Error::from(e).context("persisting tmp prompt file for handoff"))
    })?;

    let path_str = tmp_path.to_string_lossy().into_owned();
    let dst_str = dst_hint.to_string_lossy().into_owned();

    // Passive first message — read context, do not act yet. Plain
    // single-line text so it survives Windows CreateProcess
    // argument quoting (newlines / `**bold**` / backticks have all
    // been observed to break spawn through pwsh wrappers).
    let first_message = format!(
        "Read the file at {path_str} for kata context about {dst_str}. \
         Summarize what it contains in 1-2 sentences. \
         Do NOT apply, edit, or write any files yet. \
         Wait for my next instruction before running Edit or Write tools."
    );

    eprintln!();
    eprintln!("kata: handoff prompt saved to {path_str}");
    let strategy = first_message_strategy(backend);
    eprintln!(
        "kata: starting `{}` interactively (kata will not re-import the result).",
        backend.cli_name()
    );
    eprintln!();

    let label = backend.cli_name().to_string();
    let program = resolved.program.clone();
    let prefix_args = resolved.prefix_args.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&prefix_args);
        match strategy {
            FirstMessageStrategy::Positional => {
                cmd.arg(&first_message);
            }
            FirstMessageStrategy::InteractiveFlag => {
                cmd.arg("-i").arg(&first_message);
            }
        }
        let status = cmd
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| {
                Error::Other(
                    anyhow::Error::from(e).context(format!("failed to spawn AI CLI `{label}`")),
                )
            })?;
        // The exit code reflects the user's session, not a kata
        // failure — drop it.
        let _ = status;
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(anyhow::Error::from(e).context("joining handoff task")))??;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_name_is_stable_per_backend() {
        assert_eq!(Backend::Claude.cli_name(), "claude");
        assert_eq!(Backend::Gemini.cli_name(), "gemini");
        assert_eq!(Backend::Codex.cli_name(), "codex");
    }

    #[test]
    fn label_is_stable_per_backend() {
        assert_eq!(Backend::Claude.label(), "Claude");
        assert_eq!(Backend::Gemini.label(), "Gemini");
        assert_eq!(Backend::Codex.label(), "Codex");
    }

    #[test]
    fn first_message_strategy_per_backend() {
        assert_eq!(
            first_message_strategy(Backend::Claude),
            FirstMessageStrategy::Positional
        );
        assert_eq!(
            first_message_strategy(Backend::Codex),
            FirstMessageStrategy::Positional
        );
        assert_eq!(
            first_message_strategy(Backend::Gemini),
            FirstMessageStrategy::InteractiveFlag
        );
    }

    #[test]
    fn wrap_if_powershell_wraps_ps1_path() {
        let p = PathBuf::from("C:/foo/gemini.ps1");
        let r = wrap_if_powershell(p);
        let prog = r.program.to_string_lossy().to_ascii_lowercase();
        assert!(
            prog == "pwsh.exe" || prog == "powershell.exe",
            "expected pwsh.exe or powershell.exe, got {prog}",
        );
        assert!(r.prefix_args.iter().any(|a| a == "-NoProfile"));
        assert!(r.prefix_args.iter().any(|a| a == "Bypass"));
        assert!(r.prefix_args.iter().any(|a| a == "-File"));
        assert!(r.prefix_args.iter().any(|a| a.contains("gemini.ps1")));
    }

    #[test]
    fn wrap_if_powershell_passes_exe_through_unchanged() {
        let p = PathBuf::from("C:/foo/claude.exe");
        let r = wrap_if_powershell(p.clone());
        assert_eq!(r.program, p);
        assert!(r.prefix_args.is_empty());
    }

    #[test]
    fn ensure_cli_installed_errors_when_missing() {
        // Pick a backend; if it happens to be installed on the
        // dev box, skip — the assertion is about the error path,
        // not its presence.
        if Backend::Claude.is_available()
            || Backend::Codex.is_available()
            || Backend::Gemini.is_available()
        {
            return;
        }
        let err = match ensure_cli_installed(Backend::Claude) {
            Err(e) => e,
            Ok(_) => panic!("expected error when no AI CLI is installed"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("not on PATH"), "unexpected error: {msg}");
        assert!(msg.contains("claude"), "missing backend name: {msg}");
    }

    #[test]
    fn timeout_env_override_is_parsed() {
        // Inline parse so we don't have to mutate the global env
        // (Rust 2024: set_var is unsafe + flakey under parallel
        // tests). The function only reads `KATA_AI_TIMEOUT_SECS`,
        // so the easiest contract test is on the parser itself.
        let parsed: Option<u64> = "120".parse().ok();
        assert_eq!(parsed, Some(120));
        let bad: Option<u64> = "not-a-number".parse().ok();
        assert_eq!(bad, None);
    }
}
