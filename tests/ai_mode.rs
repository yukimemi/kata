//! Phase 3-b2 dispatch: `how = "ai"` files only run when the user
//! opted in (`--yes`) AND an agent is available. Every other path
//! must skip cleanly without erroring out the rest of the apply
//! run.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

fn write_preset(td: &Path, name: &str, body: &str) -> PathBuf {
    let presets_dir = td.join("presets");
    std::fs::create_dir_all(&presets_dir).unwrap();
    let path = presets_dir.join(format!("{name}.toml"));
    write(&path, body);
    path
}

fn kata(td: &Path) -> Command {
    let mut c = Command::cargo_bin("kata").unwrap();
    c.env("KATA_HOME", td.join("kata-home"))
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG");
    c
}

/// Build a tiny template with one `how = "ai"` file. The src is
/// only used to seed `<kata:incoming>` — the agent never sees it
/// during the dispatch tests, but the runner still needs the file
/// present on disk because every mode goes through the read-src
/// step before dispatch.
fn make_ai_template(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join("templates").join(name);
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root.join("template.toml"),
        r#"
name = "ai-demo"

[[file]]
src = "AGENTS.md"
how = "ai"
when = "always"
prompt = "Merge the freshly-rendered AGENTS.md into the existing one."
"#,
    );
    write(&root.join("AGENTS.md"), "kata-managed AGENTS.md\n");
    root
}

fn init_with_template(td: &Path, template: &Path, extra: &[&str]) -> PathBuf {
    let preset = write_preset(
        td,
        "default",
        &format!(
            r#"
name = "default"
[[templates]]
source = "{}"
"#,
            template.to_string_lossy().replace('\\', "/")
        ),
    );
    let pj = td.join("demo");
    let mut cmd = kata(td);
    cmd.args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive");
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.assert().success();
    pj
}

#[test]
fn no_ai_flag_skips_ai_files_without_running_agent() {
    // `--no-ai` short-circuits the agent factory before any spawn
    // attempt. Even with a `--yes` (which would normally accept an
    // AI-produced body) the file should be skipped.
    let td = TempDir::new().unwrap();
    let tmpl = make_ai_template(td.path(), "ai-demo");
    let pj = init_with_template(td.path(), &tmpl, &["--no-ai", "--yes"]);

    // The destination must NOT have been created — `--no-ai` skips.
    assert!(
        !pj.join("AGENTS.md").exists(),
        "--no-ai must skip how=ai files (AGENTS.md was created)",
    );
}

#[test]
fn ai_files_skip_without_yes_flag_even_when_agent_exists() {
    // Default `kata init --non-interactive` is the safe path:
    // skip every `how = "ai"` file so a CI run never blocks on a
    // missing prompt and never silently accepts an AI body.
    let td = TempDir::new().unwrap();
    let tmpl = make_ai_template(td.path(), "ai-demo");
    let pj = init_with_template(td.path(), &tmpl, &[]);

    assert!(
        !pj.join("AGENTS.md").exists(),
        "non-interactive init without --yes must not write the AI dst",
    );
}

#[test]
fn ai_with_yes_but_no_cli_on_path_skips_with_error_message() {
    // `--ai claude --yes` opts in, but if no `claude` is on PATH
    // the agent factory returns `None` and the mode reports a
    // skipped outcome carrying the hint. The whole apply run still
    // succeeds — failures of optional AI files do not abort the
    // rest of the templates.
    let td = TempDir::new().unwrap();
    let tmpl = make_ai_template(td.path(), "ai-demo");

    let preset = write_preset(
        td.path(),
        "default",
        &format!(
            r#"
name = "default"
[[templates]]
source = "{}"
"#,
            tmpl.to_string_lossy().replace('\\', "/")
        ),
    );
    let pj = td.path().join("demo");

    let mut cmd = kata(td.path());
    // Wipe PATH so `which::which("claude")` definitely fails.
    cmd.env("PATH", "")
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .args(["--ai", "claude", "--yes"]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("no AI agent available"));

    assert!(
        !pj.join("AGENTS.md").exists(),
        "no agent on PATH must skip the dst, not write a placeholder",
    );
}
