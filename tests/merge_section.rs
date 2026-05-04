//! Phase 2-d end-to-end: `how = "merge-section"` writes / replaces
//! / preserves a marker-bracketed block inside the target file.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
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

/// Build a template with one merge-section file. Returns the
/// absolute path of the template directory.
fn template_with_merge_entry(parent: &Path, name: &str, src_body: &str) -> PathBuf {
    let root = parent.join("templates").join(name);
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root.join("template.toml"),
        r##"
name = "merge-demo"

[[file]]
src = "fragment.txt"
dst = ".gitignore"
how = "merge-section"
when = "always"
marker = { begin = "# >>> kata managed <<<", end = "# <<< kata managed >>>" }
"##,
    );
    write(&root.join("fragment.txt"), src_body);
    root
}

fn run_init(td: &Path, template_root: &Path, pj: &Path) {
    let preset = write_preset(
        td,
        "default",
        &format!(
            r#"
name = "default"
[[templates]]
source = "{}"
"#,
            template_root.to_string_lossy().replace('\\', "/")
        ),
    );
    kata(td)
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(pj)
        .arg("--non-interactive")
        .assert()
        .success();
}

#[test]
fn merge_section_creates_marker_block_when_dst_absent() {
    let td = TempDir::new().unwrap();
    let template = template_with_merge_entry(td.path(), "create", "/target\n");
    let pj = td.path().join("demo");

    run_init(td.path(), &template, &pj);

    let body = std::fs::read_to_string(pj.join(".gitignore")).unwrap();
    assert!(
        body.contains("# >>> kata managed <<<\n/target\n# <<< kata managed >>>\n"),
        "expected fresh marker block in .gitignore, got: {body}"
    );
}

#[test]
fn merge_section_appends_block_to_existing_file_without_markers() {
    let td = TempDir::new().unwrap();
    let template = template_with_merge_entry(td.path(), "append", "/target\n");
    let pj = td.path().join("demo");

    // Pre-existing user content with no markers — kata must append
    // (and not lose any of it).
    std::fs::create_dir_all(&pj).unwrap();
    write(&pj.join(".gitignore"), "node_modules/\n*.log\n");

    run_init(td.path(), &template, &pj);

    let body = std::fs::read_to_string(pj.join(".gitignore")).unwrap();
    assert!(
        body.starts_with("node_modules/\n*.log\n"),
        "user content should be preserved at the top: {body}"
    );
    assert!(
        body.contains("# >>> kata managed <<<\n/target\n# <<< kata managed >>>\n"),
        "kata block should be appended after user content: {body}"
    );
}

#[test]
fn merge_section_replaces_existing_block_in_place() {
    let td = TempDir::new().unwrap();
    let template = template_with_merge_entry(td.path(), "replace", "/target\n/dist\n");
    let pj = td.path().join("demo");

    // Existing file already has an out-of-date kata block AND
    // surrounding user content. After apply: user content
    // preserved, kata block contents replaced.
    std::fs::create_dir_all(&pj).unwrap();
    write(
        &pj.join(".gitignore"),
        "manual-top\n# >>> kata managed <<<\n/old\n/stale\n# <<< kata managed >>>\nmanual-bottom\n",
    );

    run_init(td.path(), &template, &pj);

    let body = std::fs::read_to_string(pj.join(".gitignore")).unwrap();
    assert!(
        body.starts_with("manual-top\n"),
        "user content above the block must be preserved: {body}"
    );
    assert!(
        body.contains("# >>> kata managed <<<\n/target\n/dist\n# <<< kata managed >>>"),
        "kata block contents should be replaced: {body}"
    );
    assert!(
        body.trim_end().ends_with("manual-bottom"),
        "user content below the block must be preserved: {body}"
    );
    assert!(
        !body.contains("/old") && !body.contains("/stale"),
        "old kata-block content should be gone: {body}"
    );
}

#[test]
fn merge_section_apply_is_idempotent() {
    let td = TempDir::new().unwrap();
    let template = template_with_merge_entry(td.path(), "idempotent", "/target\n");
    let pj = td.path().join("demo");

    run_init(td.path(), &template, &pj);

    // Re-apply should report unchanged for the merge-section file.
    use predicates::prelude::*;
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"));
}
