//! Phase 1 end-to-end coverage: `kata init` → `kata apply` →
//! `kata status` against an inline-built local preset + template.
//!
//! Each test gets its own tempdir (used for both the fixture *and*
//! `KATA_HOME` so global config doesn't leak between runs). Vars
//! that would normally be prompted are injected via
//! `KATA_VAR_<name>` to keep tests headless.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Inline fixture builder for a template directory.
struct TemplateBuilder {
    root: PathBuf,
}

impl TemplateBuilder {
    fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn manifest(self, body: &str) -> Self {
        write(&self.root.join("template.toml"), body);
        self
    }

    fn file(self, rel: &str, body: &str) -> Self {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        write(&path, body);
        self
    }
}

fn write(path: &Path, body: &str) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
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

#[test]
fn init_writes_files_with_vars_rendered() {
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"

            [vars]
            project = { prompt = "name?", required = true }

            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            when = "always"

            [[file]]
            src = "src/main.rs"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file(
            "Makefile.toml",
            "[tasks.run]\ndescription = \"run {{ vars.project }}\"\n",
        )
        .file(
            "src/main.rs",
            "fn main() { println!(\"{{ vars.project }}\"); }\n",
        );

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"

        [[templates]]
        source = "../templates/pj-base"
        "#,
    );

    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .env("KATA_VAR_project", "shun")
        .assert()
        .success();

    let makefile = std::fs::read_to_string(pj.join("Makefile.toml")).unwrap();
    assert!(makefile.contains("run shun"), "got: {makefile}");
    let mainrs = std::fs::read_to_string(pj.join("src/main.rs")).unwrap();
    assert!(mainrs.contains("println!(\"shun\")"), "got: {mainrs}");

    let applied = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(
        applied.contains("project = \"shun\""),
        "applied.toml should record vars: {applied}"
    );
    assert!(
        applied.contains("source = \""),
        "applied.toml should list templates: {applied}"
    );
}

#[test]
fn apply_is_idempotent_after_init() {
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [vars]
            project = { default = "demo" }
            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml", "name = {{ vars.project }}\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        "#,
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

    // Second apply should report "unchanged" and not error.
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"));
}

#[test]
fn once_mode_does_not_overwrite_user_edits_on_apply() {
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "src/main.rs"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("src/main.rs", "fn main() { /* template */ }\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        "#,
    );
    let pj = td.path().join("demo");

    // 1) init writes the file
    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // 2) user edits it
    write(&pj.join("src/main.rs"), "fn main() { /* user edit */ }\n");

    // 3) apply must NOT overwrite the user's edit
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let body = std::fs::read_to_string(pj.join("src/main.rs")).unwrap();
    assert!(
        body.contains("user edit"),
        "once-mode file should be preserved across apply: {body}"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [vars]
            project = { default = "demo" }
            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml", "name = {{ vars.project }}\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        "#,
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

    // Edit the rendered file out-of-band, then dry-run apply: file
    // should remain edited (dry-run never writes).
    write(&pj.join("Makefile.toml"), "edited\n");

    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .args(["--dry-run", "--non-interactive"])
        .assert()
        .success();

    let body = std::fs::read_to_string(pj.join("Makefile.toml")).unwrap();
    assert_eq!(body, "edited\n", "dry-run must not write");
}

#[test]
fn preset_compose_later_template_wins() {
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("base"))
        .manifest(
            r#"
            name = "base"
            [[file]]
            src = "X"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("X", "from-base\n");

    TemplateBuilder::new(templates.join("over"))
        .manifest(
            r#"
            name = "over"
            [[file]]
            src = "X"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("X", "from-over\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/base"
        [[templates]]
        source = "../templates/over"
        "#,
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

    let body = std::fs::read_to_string(pj.join("X")).unwrap();
    assert_eq!(body, "from-over\n", "later template must win");
}

#[test]
fn status_reports_create_for_uninitialised_files() {
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [vars]
            project = { default = "demo" }
            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml", "name = {{ vars.project }}\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        "#,
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

    // Delete the rendered file; status should re-detect Create.
    std::fs::remove_file(pj.join("Makefile.toml")).unwrap();
    kata(td.path())
        .args(["status", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("create"));
}

#[test]
fn init_refuses_dst_path_traversal() {
    // A hostile / buggy template should NOT be able to write outside
    // the project root via `dst = "../../escape"` — defence in depth
    // for the `check_relative_contained` runner guard.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("evil"))
        .manifest(
            r#"
            name = "evil"
            [[file]]
            src = "payload"
            dst = "../../escape.txt"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("payload", "owned\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/evil"
        "#,
    );
    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .failure()
        .stderr(predicate::str::contains("escapes its root"));

    // The escape file must NOT have been written.
    assert!(!td.path().join("escape.txt").exists());
}

#[test]
fn init_refuses_remote_preset_in_phase_1() {
    let td = TempDir::new().unwrap();
    let pj = td.path().join("demo");
    kata(td.path())
        .args(["init", "github.com/yukimemi/some-presets:rust-cli", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .failure()
        .stderr(predicate::str::contains("local"));
}
