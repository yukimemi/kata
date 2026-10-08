//! `kata ignore` / `kata unignore`: a consumer-side marker in
//! `.kata/applied.toml` that keeps a template-managed file absent.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::File::create(path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
}

fn kata(td: &Path) -> Command {
    let mut c = Command::cargo_bin("kata").unwrap();
    c.env("KATA_HOME", td.join("kata-home"))
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG");
    c
}

/// Two layers: `base` ships ci.yml (always), seed.txt (once) and
/// shared.txt; `top` also ships shared.txt. Returns the PJ dir after
/// `kata init`.
fn fixture(td: &Path) -> PathBuf {
    let base = td.join("templates/base");
    write(
        &base.join("template.toml"),
        r#"
name = "base"
[[file]]
src = "ci.yml"
dst = ".github/workflows/ci.yml"
how = "overwrite"
when = "always"
[[file]]
src = "seed.txt"
how = "overwrite"
when = "once"
[[file]]
src = "shared.txt"
how = "overwrite"
when = "always"
"#,
    );
    write(&base.join("ci.yml"), "name: ci\n");
    write(&base.join("seed.txt"), "seed\n");
    write(&base.join("shared.txt"), "base\n");
    let top = td.join("templates/top");
    write(
        &top.join("template.toml"),
        r#"
name = "top"
[[file]]
src = "shared.txt"
how = "overwrite"
when = "always"
"#,
    );
    write(&top.join("shared.txt"), "top\n");
    write(
        &td.join("presets/default.toml"),
        r#"
name = "default"
[[templates]]
source = "../templates/base"
[[templates]]
source = "../templates/top"
"#,
    );
    let pj = td.join("demo");
    kata(td)
        .arg("init")
        .arg(td.join("presets/default.toml"))
        .arg("--at")
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();
    pj
}

fn apply(td: &Path, pj: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
    kata(td)
        .args(["apply", "--at"])
        .arg(pj)
        .arg("--non-interactive")
        .args(extra)
        .assert()
}

fn ignore(td: &Path, pj: &Path, path: &str) -> assert_cmd::assert::Assert {
    kata(td)
        .args(["--non-interactive", "ignore", "--yes", "--at"])
        .arg(pj)
        .arg(path)
        .assert()
}

const CI: &str = ".github/workflows/ci.yml";

#[test]
fn ignore_deletes_marks_and_apply_does_not_recreate() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    assert!(pj.join(CI).exists());

    ignore(td.path(), &pj, CI).success();
    assert!(!pj.join(CI).exists());
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(applied.contains("ignored = true"), "{applied}");

    apply(td.path(), &pj, &[])
        .success()
        .stdout(predicate::str::contains("ignored"));
    assert!(!pj.join(CI).exists());
    // --reseed does not override the marker.
    apply(td.path(), &pj, &["--reseed", CI]).success();
    assert!(!pj.join(CI).exists());
}

#[test]
fn apply_leaves_ignored_existing_file_untouched() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    ignore(td.path(), &pj, CI).success();

    write(&pj.join(CI), "my own content\n");
    apply(td.path(), &pj, &[])
        .success()
        .stdout(predicate::str::contains("present on disk, untouched"));
    assert_eq!(
        std::fs::read_to_string(pj.join(CI)).unwrap(),
        "my own content\n"
    );
}

#[test]
fn dry_run_and_status_report_ignored_not_drift() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    ignore(td.path(), &pj, CI).success();

    kata(td.path())
        .args(["status", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"ignored\s+\.github/workflows/ci\.yml").unwrap())
        .stdout(predicate::str::contains("create").not());

    write(&pj.join(CI), "edited\n");
    kata(td.path())
        .args(["status", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("present on disk, untouched"));

    kata(td.path())
        .args(["list", "--at"])
        .arg(&pj)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            ".github/workflows/ci.yml (ignored)",
        ));
}

#[test]
fn ignored_dst_blocks_every_layer() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    ignore(td.path(), &pj, "shared.txt").success();
    assert!(!pj.join("shared.txt").exists());
    apply(td.path(), &pj, &[]).success();
    assert!(!pj.join("shared.txt").exists());
}

#[test]
fn ignored_once_file_survives_reseed() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    ignore(td.path(), &pj, "seed.txt").success();
    apply(td.path(), &pj, &["--reseed", "seed.txt"]).success();
    assert!(!pj.join("seed.txt").exists());
}

#[test]
fn unignore_restores_on_next_apply() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    ignore(td.path(), &pj, CI).success();
    apply(td.path(), &pj, &[]).success();
    assert!(!pj.join(CI).exists());

    kata(td.path())
        .args(["unignore", "--at"])
        .arg(&pj)
        .arg(CI)
        .assert()
        .success();
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(!applied.contains("ignored"), "{applied}");

    apply(td.path(), &pj, &[]).success();
    assert_eq!(std::fs::read_to_string(pj.join(CI)).unwrap(), "name: ci\n");
}

#[test]
fn unignore_of_non_ignored_path_fails() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    kata(td.path())
        .args(["unignore", "--at"])
        .arg(&pj)
        .arg(CI)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not ignored"));
}

#[test]
fn ignore_without_yes_in_non_interactive_mode_errors_and_keeps_file() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    kata(td.path())
        .args(["--non-interactive", "ignore", "--at"])
        .arg(&pj)
        .arg(CI)
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
    assert!(pj.join(CI).exists());
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(!applied.contains("ignored"), "{applied}");
}

#[test]
fn ignore_rejects_unknown_outside_and_state_paths() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    ignore(td.path(), &pj, "no/such/file.txt").failure();
    ignore(td.path(), &pj, "../escape.txt").failure();
    ignore(td.path(), &pj, ".kata/applied.toml").failure();
    ignore(td.path(), &pj, ".github").failure();
    assert!(pj.join(CI).exists());
}

#[test]
fn repos_without_marker_behave_as_before() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    std::fs::remove_file(pj.join(CI)).unwrap();
    apply(td.path(), &pj, &[]).success();
    assert!(pj.join(CI).exists());
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(!applied.contains("ignored"), "{applied}");
}

#[test]
fn equivalent_dst_spellings_are_all_ignored() {
    let td = TempDir::new().unwrap();
    let pj = fixture(td.path());
    // A second layer spelling the same dst as `./shared.txt`.
    let top = td.path().join("templates/top");
    write(
        &top.join("template.toml"),
        r#"
name = "top"
[[file]]
src = "shared.txt"
dst = "./shared.txt"
how = "overwrite"
when = "always"
"#,
    );
    ignore(td.path(), &pj, "shared.txt").success();
    apply(td.path(), &pj, &[]).success();
    assert!(!pj.join("shared.txt").exists());
}

#[test]
fn ignored_vars_seed_with_broken_toml_does_not_fail() {
    let td = TempDir::new().unwrap();
    let base = td.path().join("templates/base");
    write(
        &base.join("template.toml"),
        r#"
name = "base"
[[file]]
src = "vars.toml"
dst = ".kata/vars.toml"
how = "overwrite"
when = "once"
"#,
    );
    write(&base.join("vars.toml"), "x = 1\n");
    write(
        &td.path().join("presets/default.toml"),
        "name = \"default\"\n[[templates]]\nsource = \"../templates/base\"\n",
    );
    let pj = td.path().join("demo");
    kata(td.path())
        .arg("init")
        .arg(td.path().join("presets/default.toml"))
        .arg("--at")
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();
    ignore(td.path(), &pj, ".kata/vars.toml").success();
    write(&base.join("vars.toml"), "x = [\n");
    apply(td.path(), &pj, &[]).success();
    kata(td.path())
        .args(["status", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();
}
