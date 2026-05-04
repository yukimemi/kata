//! Phase 2-g end-to-end smoke for `kata add` / `remove` / `update`.

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

/// Build a tiny local template at `<parent>/templates/<name>` with
/// one always-mode file.
fn make_local_template(parent: &Path, name: &str, file_body: &str) -> PathBuf {
    let root = parent.join("templates").join(name);
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root.join("template.toml"),
        r#"
name = "demo"
[[file]]
src = "marker.txt"
how = "overwrite"
when = "always"
"#,
    );
    write(&root.join("marker.txt"), file_body);
    root
}

fn init_with_one_template(td: &Path, template: &Path) -> PathBuf {
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
    kata(td)
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();
    pj
}

#[test]
fn add_appends_template_and_lands_its_files() {
    let td = TempDir::new().unwrap();
    let first = make_local_template(td.path(), "first", "from-first\n");
    let pj = init_with_one_template(td.path(), &first);

    // Sanity: only the first template's marker landed.
    let body = std::fs::read_to_string(pj.join("marker.txt")).unwrap();
    assert!(body.contains("from-first"), "init: {body}");

    // Add a second local template that writes the same dst — last
    // wins, so marker.txt should now read from the new one.
    let second = make_local_template(td.path(), "second", "from-second\n");
    kata(td.path())
        .args(["add"])
        .arg(second.to_string_lossy().replace('\\', "/"))
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let body = std::fs::read_to_string(pj.join("marker.txt")).unwrap();
    assert!(
        body.contains("from-second"),
        "after add, latest template should win: {body}"
    );

    // applied.toml should now record both templates.
    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    let occ = applied.matches("[[templates]]").count();
    assert_eq!(occ, 2, "expected 2 templates in applied.toml: {applied}");
}

#[test]
fn add_refuses_duplicate_source() {
    let td = TempDir::new().unwrap();
    let only = make_local_template(td.path(), "only", "x\n");
    let pj = init_with_one_template(td.path(), &only);

    // Adding the same source again is an error.
    kata(td.path())
        .args(["add"])
        .arg(only.to_string_lossy().replace('\\', "/"))
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .failure()
        .stderr(predicate::str::contains("already applied"));
}

#[test]
fn remove_drops_template_from_applied_toml() {
    let td = TempDir::new().unwrap();
    let only = make_local_template(td.path(), "only", "x\n");
    let pj = init_with_one_template(td.path(), &only);

    let source = only.to_string_lossy().replace('\\', "/");
    kata(td.path())
        .args(["remove"])
        .arg(&source)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(
        !applied.contains("[[templates]]"),
        "applied.toml should have no templates after remove: {applied}"
    );
    // The file the template wrote should still be there
    // (Phase 2-g remove is intentionally non-destructive of files).
    assert!(pj.join("marker.txt").exists());
}

#[test]
fn remove_errors_when_template_not_present() {
    let td = TempDir::new().unwrap();
    let only = make_local_template(td.path(), "only", "x\n");
    let pj = init_with_one_template(td.path(), &only);

    kata(td.path())
        .args(["remove", "no-such-template"])
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not applied"));
}

#[test]
fn update_skips_local_templates() {
    let td = TempDir::new().unwrap();
    let only = make_local_template(td.path(), "only", "x\n");
    let pj = init_with_one_template(td.path(), &only);

    // Local templates are skipped — no fetch happens, no rev change.
    kata(td.path())
        .args(["update", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("skip local"));
}
