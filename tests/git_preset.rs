//! End-to-end: a `PresetSpec` pointing at a real git `file://`
//! URL gets cloned into the kata template cache and its `[[templates]]`
//! list applied to the project. Validates Phase 2-c2.

use std::io::Write;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(body.as_bytes()).unwrap();
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
    assert!(status.success(), "git {} exited non-zero", args.join(" "));
}

fn git_init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git_in(dir, &["init", "-b", "main"]);
    git_in(dir, &["config", "user.email", "test@kata.test"]);
    git_in(dir, &["config", "user.name", "kata-test"]);
    git_in(dir, &["config", "commit.gpgsign", "false"]);
}

fn file_url(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        // Windows (`C:/Users/...`) needs the extra `/` after the
        // scheme to give an empty authority.
        format!("file:///{s}")
    }
}

fn kata(td: &Path) -> Command {
    let mut c = Command::cargo_bin("kata").unwrap();
    c.env("KATA_HOME", td.join("kata-home"))
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG");
    c
}

#[test]
fn init_resolves_git_preset_and_applies_its_templates() {
    // Single git repo containing both the preset file and the
    // template directories it references. Real-world setups are
    // more likely to split preset / template across repos
    // (covered by the second test below), but this single-repo
    // form still exercises the new git-preset code path with the
    // smallest possible fixture.
    let td = TempDir::new().unwrap();
    let upstream = td.path().join("preset-repo");
    git_init_repo(&upstream);

    // pj-base template inside the same repo
    write(
        &upstream.join("pj-base/template.toml"),
        r#"
name = "pj-base"
[[file]]
src = "LICENSE"
how = "overwrite"
when = "once"
"#,
    );
    write(&upstream.join("pj-base/LICENSE"), "MIT — sample\n");

    // The preset file. `source = "./pj-base"` is relative to the
    // preset file itself (= the cache slot root after fetch).
    write(
        &upstream.join("rust-cli.toml"),
        r#"
name = "rust-cli"
[[templates]]
source = "./pj-base"
"#,
    );

    git_in(&upstream, &["add", "-A"]);
    git_in(&upstream, &["commit", "-q", "-m", "init"]);

    let preset_spec = format!("{}:rust-cli", file_url(&upstream));
    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset_spec)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // Template's once-mode LICENSE landed in the PJ.
    let license = std::fs::read_to_string(pj.join("LICENSE")).unwrap();
    assert!(license.contains("MIT — sample"), "got: {license}");

    // applied.toml records the spec verbatim, so re-apply works.
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(
        applied.contains(&preset_spec) || applied.contains("rust-cli"),
        "applied.toml should reference the preset: {applied}"
    );
}

#[test]
fn init_resolves_git_preset_with_git_template_sources() {
    // The realistic shape: a `pj-presets` repo whose preset
    // points at *separate* `pj-base` etc. repos via git URL.
    let td = TempDir::new().unwrap();
    let templates_repo = td.path().join("pj-base-repo");
    let preset_repo = td.path().join("pj-presets-repo");

    // 1) the template repo
    git_init_repo(&templates_repo);
    write(
        &templates_repo.join("template.toml"),
        r#"
name = "pj-base"
[[file]]
src = "LICENSE"
how = "overwrite"
when = "once"
"#,
    );
    write(
        &templates_repo.join("LICENSE"),
        "MIT — from-template-repo\n",
    );
    git_in(&templates_repo, &["add", "-A"]);
    git_in(&templates_repo, &["commit", "-q", "-m", "init"]);
    let template_url = file_url(&templates_repo);

    // 2) the preset repo, pointing at the template repo by URL
    git_init_repo(&preset_repo);
    write(
        &preset_repo.join("rust-cli.toml"),
        &format!(
            r#"
name = "rust-cli"
[[templates]]
source = "{template_url}"
"#
        ),
    );
    git_in(&preset_repo, &["add", "-A"]);
    git_in(&preset_repo, &["commit", "-q", "-m", "init"]);
    let preset_spec = format!("{}:rust-cli", file_url(&preset_repo));

    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset_spec)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let license = std::fs::read_to_string(pj.join("LICENSE")).unwrap();
    assert!(
        license.contains("from-template-repo"),
        "git preset → git template chain should land the right file: {license}"
    );

    // Two cache slots should now exist (preset + template); a third
    // would imply something accidentally re-cloned.
    let cache_root = td.path().join("kata-home").join("cache").join("templates");
    let slot_count = std::fs::read_dir(&cache_root)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        slot_count, 2,
        "expected exactly 2 cache slots (preset + template)"
    );
}

#[test]
fn init_remote_preset_no_longer_errors_with_phase_1_message() {
    // Smoke: the old "Phase 1 supports local presets only" error
    // is gone — a remote preset that simply doesn't resolve
    // (bogus URL) should fail at the git-clone step, not at the
    // local-only gate.
    let td = TempDir::new().unwrap();
    let pj = td.path().join("demo");

    let stderr = kata(td.path())
        .args([
            "init",
            "https://kata-test.invalid/does-not-exist:rust-cli",
            "--at",
        ])
        .arg(&pj)
        .arg("--non-interactive")
        .output()
        .unwrap()
        .stderr;
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        !stderr.contains("Phase 1 supports local presets only"),
        "the Phase 1 gate should be gone: {stderr}"
    );
}
