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

    // Files containing Tera syntax must use the `.tera` suffix to opt
    // into rendering; the suffix is stripped on the destination side
    // unless `dst` is set explicitly.
    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"

            [vars]
            project = { prompt = "name?", required = true }

            [[file]]
            src = "Makefile.toml.tera"
            how = "overwrite"
            when = "always"

            [[file]]
            src = "src/main.rs.tera"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file(
            "Makefile.toml.tera",
            "[tasks.run]\ndescription = \"run {{ vars.project }}\"\n",
        )
        .file(
            "src/main.rs.tera",
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
            src = "Makefile.toml.tera"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml.tera", "name = {{ vars.project }}\n");

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
            src = "Makefile.toml.tera"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml.tera", "name = {{ vars.project }}\n");

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
            src = "Makefile.toml.tera"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml.tera", "name = {{ vars.project }}\n");

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
fn apply_resolves_template_sources_relative_to_recorded_base_dir() {
    // Reproducer for the "kata apply fails when pj isn't a sibling of
    // templates" bug found during Phase 1 dogfood. The preset records
    // a relative `source = "../templates/pj-base"`, and apply must
    // resolve that against the preset's directory (recorded in
    // applied.toml.base_dir), NOT the project's cwd.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "Makefile.toml"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("Makefile.toml", "ok\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        "#,
    );

    // Place the PJ in a NESTED subdir so its parent has no `templates/`.
    // Pre-fix, apply resolved "../templates/pj-base" against the pj's
    // cwd → walked into `nested/inner/templates/pj-base` → not found.
    let pj = td.path().join("nested").join("inner").join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // applied.toml must record the resolution base.
    let applied_body = std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap();
    assert!(
        applied_body.contains("base_dir"),
        "applied.toml should record base_dir for re-resolution: {applied_body}"
    );

    // Re-apply must succeed using the recorded base_dir, not cwd.
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"));
}

#[test]
fn tera_opt_in_renders_only_dot_tera_files() {
    // Pin the `.tera` opt-in convention: a file with the suffix is
    // rendered and the suffix is stripped on dst; a sibling file
    // *without* the suffix passes through byte-for-byte even when
    // its body looks like Tera (`{{ ... }}` / GitHub Actions
    // `${{ ... }}`). This is what makes ci.yml / Mustache files
    // safe to ship in templates without `{% raw %}` wrappers.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [vars]
            project = { default = "demo" }

            # Renders: .tera suffix opts in; dst auto-strips to "rendered.txt"
            [[file]]
            src = "rendered.txt.tera"
            how = "overwrite"
            when = "always"

            # Literal copy: no .tera suffix, even with Tera-looking body
            [[file]]
            src = "literal.yml"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("rendered.txt.tera", "Hello, {{ vars.project }}!\n")
        .file(
            "literal.yml",
            "group: ${{ github.workflow }}-{{ vars.project }}\n",
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
        .assert()
        .success();

    // Rendered: vars.project substituted, .tera stripped from dst.
    let rendered = std::fs::read_to_string(pj.join("rendered.txt")).unwrap();
    assert_eq!(rendered, "Hello, demo!\n", "got: {rendered}");
    assert!(
        !pj.join("rendered.txt.tera").exists(),
        "the .tera src should not appear at dst"
    );

    // Literal: byte-identical to source; Tera syntax untouched even
    // though `{{ vars.project }}` would render to "demo" if it had
    // been processed.
    let literal = std::fs::read_to_string(pj.join("literal.yml")).unwrap();
    assert_eq!(
        literal, "group: ${{ github.workflow }}-{{ vars.project }}\n",
        "literal copy must NOT process Tera syntax: {literal}"
    );
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

// `init_refuses_remote_preset_in_phase_1` retired — Phase 2-c2
// turned this into a supported case. End-to-end git-preset coverage
// is in `tests/git_preset.rs`.

#[test]
fn unchanged_files_are_recorded_in_applied_toml() {
    // Issue #16: unchanged files should be recorded in applied.toml
    // so that `when = "once"` guard and drift detection work correctly.
    //
    // Strategy: Run `kata init`, then REMOVE the [files] section
    // from applied.toml to simulate the bug (files written but not
    // tracked). Re-create files with exact template content and
    // re-apply. The second apply will see them as "unchanged" and
    // (after fix) must record them.
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
            [[file]]
            src = "LICENSE"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("Makefile.toml", "name = {{ vars.project }}\n")
        .file("LICENSE", "MIT\n");

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

    // 1) init writes the files and records them in applied.toml
    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // 2) Remove [files] section from applied.toml to simulate the
    //    bug where unchanged files were not recorded
    let applied_path = pj.join(".kata/applied.toml");
    let applied_body = std::fs::read_to_string(&applied_path).unwrap();
    // Remove the [files] section
    let new_body: String = applied_body
        .lines()
        .filter(|line| !line.trim_start().starts_with("[files"))
        .filter(|line| !line.trim_start().starts_with("content_hash"))
        .filter(|line| !line.trim_start().starts_with("once_applied"))
        .collect::<Vec<_>>()
        .join("\n");
    write(&applied_path, &new_body);

    // 3) Re-create files with exact template content.
    //    Makefile.toml has no .tera suffix, so render_or_passthrough
    //    returns the raw template body "name = {{ vars.project }}\n".
    //    Write that exact body so apply sees an identical file and
    //    triggers OutcomeKind::Unchanged.
    write(&pj.join("Makefile.toml"), "name = {{ vars.project }}\n");
    write(&pj.join("LICENSE"), "MIT\n");

    // 4) Re-apply (files will be "unchanged" but must be recorded)
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // 5) applied.toml MUST record both files
    let applied_body = std::fs::read_to_string(&applied_path).unwrap();

    // Parse applied.toml to verify
    let applied: toml::Table = toml::from_str(&applied_body).unwrap();
    let files = applied["files"].as_table().expect("files table must exist");

    assert!(
        files.contains_key("Makefile.toml"),
        "Makefile.toml should be recorded in applied.toml even when unchanged: {applied_body}"
    );
    assert!(
        files.contains_key("LICENSE"),
        "LICENSE should be recorded in applied.toml even when unchanged: {applied_body}"
    );

    // content_hash must be populated
    let makefile_state = files["Makefile.toml"].as_table().unwrap();
    assert!(
        makefile_state.contains_key("content_hash"),
        "Makefile.toml must have content_hash: {makefile_state:?}"
    );

    let license_state = files["LICENSE"].as_table().unwrap();
    assert!(
        license_state.contains_key("content_hash"),
        "LICENSE must have content_hash: {license_state:?}"
    );

    // once_applied must be true for LICENSE (when = "once")
    let once_applied = license_state
        .get("once_applied")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(
        once_applied,
        "LICENSE (when = once) must have once_applied = true"
    );
}

#[test]
fn once_file_unchanged_on_first_apply_still_guards_later() {
    // Issue #16 follow-up: if a `when = "once"` file lands as
    // unchanged on the very first apply, the once guard must still
    // fire on subsequent applies when the user edits the file.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "README.md"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("README.md", "# Demo\n");

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

    // 1) init (file written)
    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // 2) Edit the file (simulate user edit)
    write(&pj.join("README.md"), "# User Edit\n");

    // 3) Re-apply: once guard should fire, NOT the re-write path
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // 4) User edit must be preserved
    let body = std::fs::read_to_string(pj.join("README.md")).unwrap();
    assert!(
        body.contains("User Edit"),
        "once-mode file should be preserved when guard fires: {body}"
    );
}
