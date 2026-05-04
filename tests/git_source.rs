//! End-to-end: a `TemplateRef.source` pointing at a real git
//! `file://` URL gets cloned into the kata template cache and
//! applied to the project. Validates the Phase 2-c1 git path
//! end-to-end (git CLI shell-out + cache slot + clone-on-first-use
//! + commit-SHA round-trip into applied.toml).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(body.as_bytes()).unwrap();
}

/// Initialise a real git repo with a kata template inside, ready to
/// be referenced via a `file://` URL.
fn make_remote_template(parent: &Path) -> PathBuf {
    let upstream = parent.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();

    let git = |args: &[&str]| {
        let status = StdCommand::new("git")
            .current_dir(&upstream)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(status.success(), "git {} exited non-zero", args.join(" "));
    };

    git(&["init", "-b", "main"]);
    // Local-only identity so commits succeed without a global config.
    git(&["config", "user.email", "test@kata.test"]);
    git(&["config", "user.name", "kata-test"]);
    git(&["config", "commit.gpgsign", "false"]);

    write(
        &upstream.join("template.toml"),
        r#"
name = "remote-pj"
version = "0.1.0"

[[file]]
src = "Makefile.toml"
how = "overwrite"
when = "always"
"#,
    );
    write(
        &upstream.join("Makefile.toml"),
        "[tasks.run]\ncommand = \"echo\"\nargs = [\"from-git\"]\n",
    );

    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);

    upstream
}

/// Convert an OS path to the URL form `git` accepts:
/// `file:///C:/...` on Windows, `file:///abs/path` on Unix.
fn file_url(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        // Windows (`C:/Users/...`) — needs the extra `/` after the
        // scheme to give an empty authority.
        format!("file:///{s}")
    }
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

#[test]
fn init_can_fetch_template_from_git_file_url() {
    let td = TempDir::new().unwrap();
    let upstream = make_remote_template(td.path());
    let upstream_url = file_url(&upstream);

    let preset = write_preset(
        td.path(),
        "default",
        &format!(
            r#"
            name = "default"
            [[templates]]
            source = "{upstream_url}"
            "#
        ),
    );
    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("Makefile.toml"));

    // The cloned template's file landed.
    let body = std::fs::read_to_string(pj.join("Makefile.toml")).unwrap();
    assert!(body.contains("from-git"), "got: {body}");

    // applied.toml records the resolved commit SHA (full hex), NOT
    // the symbolic ref "main" or "local". Phase 2-c1's contract.
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    let rev_line = applied
        .lines()
        .find(|l| l.trim_start().starts_with("rev = \""))
        .expect("applied.toml should record a `rev = \"...\"` line");
    let sha = rev_line
        .split('"')
        .nth(1)
        .expect("rev value should be quoted");
    assert!(
        sha.len() >= 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
        "rev should be a full commit SHA (40 hex chars), got `{sha}`"
    );

    // Cache slot exists under KATA_HOME (so we didn't accidentally
    // clone into the user's real ~/.cache/).
    let cache_root = td.path().join("kata-home").join("cache").join("templates");
    let slots: Vec<_> = std::fs::read_dir(&cache_root)
        .unwrap_or_else(|e| panic!("read cache root {}: {e}", cache_root.display()))
        .flatten()
        .collect();
    assert_eq!(
        slots.len(),
        1,
        "expected exactly one cache slot under {}",
        cache_root.display()
    );
}

#[test]
fn re_apply_reuses_cache_slot_without_re_cloning() {
    // Cache contract: once a slot exists, kata trusts it. Phase
    // 2-g's `kata update` will be the explicit refresh path.
    let td = TempDir::new().unwrap();
    let upstream = make_remote_template(td.path());
    let upstream_url = file_url(&upstream);

    let preset = write_preset(
        td.path(),
        "default",
        &format!(
            r#"
            name = "default"
            [[templates]]
            source = "{upstream_url}"
            "#
        ),
    );
    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // Mutate the upstream after init to *prove* re-apply doesn't
    // re-clone (would otherwise pull the new content).
    write(
        &upstream.join("Makefile.toml"),
        "[tasks.run]\ncommand = \"would-be-fetched\"\n",
    );
    let git = |args: &[&str]| {
        StdCommand::new("git")
            .current_dir(&upstream)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
    };
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "evolve"]);

    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"));

    let body = std::fs::read_to_string(pj.join("Makefile.toml")).unwrap();
    assert!(
        body.contains("from-git"),
        "re-apply should reuse the cached snapshot, not re-fetch: {body}"
    );
}
