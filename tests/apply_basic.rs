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

    // `Makefile.toml` is `when = "always"` — drift detection
    // applies, so content_hash must be populated.
    let makefile_state = files["Makefile.toml"].as_table().unwrap();
    assert!(
        makefile_state.contains_key("content_hash"),
        "Makefile.toml (when = always) must have content_hash: {makefile_state:?}"
    );

    // `LICENSE` is `when = "once"` — consumer's free zone after
    // first write. We deliberately don't record content_hash so
    // `kata status` doesn't emit drift on every later consumer
    // edit. `once_applied = true` is what guards subsequent
    // applies; content_hash being absent is expected.
    let license_state = files["LICENSE"].as_table().unwrap();
    assert!(
        !license_state.contains_key("content_hash"),
        "LICENSE (when = once) should NOT have content_hash recorded: {license_state:?}"
    );

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

#[test]
fn once_existing_file_is_adopted_not_overwritten() {
    // Issue #37: when `kata init` is run against a project that
    // already has a file matching a `when = "once"` entry, the
    // pre-existing content must be kept and `once_applied = true`
    // recorded — adoption flow, not destructive overwrite.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "tailwind.config.js"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file(
            "tailwind.config.js",
            "export default { theme: { extend: {} } };\n",
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
    std::fs::create_dir_all(&pj).unwrap();

    // 1) Pre-existing project content (e.g. project-specific
    //    Tailwind theme that the consumer wrote before adopting
    //    kata).
    let consumer_body = "export default { theme: { extend: { colors: { brand: '#A52A1F' } } } };\n";
    write(&pj.join("tailwind.config.js"), consumer_body);

    // 2) `kata init` against the existing project.
    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // 3) The consumer's content must survive — no overwrite.
    let body = std::fs::read_to_string(pj.join("tailwind.config.js")).unwrap();
    assert_eq!(
        body, consumer_body,
        "pre-existing once-mode file must be adopted as-is, not overwritten"
    );

    // 4) `applied.toml` must record `once_applied = true` so
    //    subsequent applies keep skipping.
    let applied_path = pj.join(".kata/applied.toml");
    let applied_body = std::fs::read_to_string(&applied_path).unwrap();
    let applied_doc: toml::Table = toml::from_str(&applied_body).unwrap();
    let files = applied_doc["files"].as_table().unwrap();
    let state = files["tailwind.config.js"].as_table().unwrap();
    assert_eq!(
        state.get("once_applied").and_then(|v| v.as_bool()),
        Some(true),
        "adopted file must be marked once_applied = true: {state:?}"
    );

    // 5) content_hash must NOT be recorded for once entries
    //    (consumer's free zone — drift would just emit noise).
    assert!(
        !state.contains_key("content_hash"),
        "adopted (once) file should NOT have content_hash: {state:?}"
    );
}

#[test]
fn once_does_not_adopt_a_directory_at_dst() {
    // Adoption must only fire when the destination is a regular
    // file. A directory at `dst` is an invalid template
    // destination shape that we shouldn't permanently mask
    // behind `once_applied = true` (CodeRabbit review feedback
    // on PR #38).
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "config.js"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("config.js", "// template default\n");

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
    std::fs::create_dir_all(&pj).unwrap();

    // Pre-existing *directory* (not a file) at the once-target
    // path. Adoption must refuse, not silently mark it
    // `once_applied`.
    std::fs::create_dir_all(pj.join("config.js")).unwrap();

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .failure();

    // The directory must still be there (kata didn't overwrite
    // it), and `applied.toml` must NOT have flagged config.js
    // as `once_applied = true` — leaving the destination shape
    // free to be diagnosed and fixed.
    assert!(
        pj.join("config.js").is_dir(),
        "directory at dst should be left intact"
    );
    let applied_path = pj.join(".kata/applied.toml");
    if applied_path.exists() {
        let applied_body = std::fs::read_to_string(&applied_path).unwrap();
        let applied_doc: toml::Table = toml::from_str(&applied_body).unwrap();
        if let Some(files) = applied_doc.get("files").and_then(|v| v.as_table()) {
            if let Some(state) = files.get("config.js").and_then(|v| v.as_table()) {
                let once_applied = state
                    .get("once_applied")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                assert!(
                    !once_applied,
                    "directory at dst must not be flagged once_applied: {state:?}"
                );
            }
        }
    }
}

#[test]
fn once_adoption_makes_subsequent_apply_a_noop() {
    // After adoption, `kata apply` must keep skipping the file
    // even if the consumer edits it further — same guarantee as
    // the standard "once was written, then edited" flow.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "config.js"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("config.js", "// template default\n");

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
    std::fs::create_dir_all(&pj).unwrap();

    // Pre-existing consumer file before adoption.
    write(&pj.join("config.js"), "// consumer original\n");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // Consumer continues to edit after adoption.
    write(&pj.join("config.js"), "// consumer edited later\n");

    // Re-apply must still skip — once_applied flag wins.
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let body = std::fs::read_to_string(pj.join("config.js")).unwrap();
    assert_eq!(
        body, "// consumer edited later\n",
        "subsequent apply must not touch an adopted once file"
    );
}

#[test]
fn once_status_does_not_threaten_to_overwrite_consumer_edits() {
    // Issue #37 design follow-on: `once` is the consumer's free
    // zone after first write. After a consumer edits a once
    // file, `kata status` must show `skip(once)` (not `update`)
    // so the consumer isn't told kata is about to overwrite
    // their work.
    //
    // The `always` sibling is the control: it must show
    // `update` to confirm `status` does flag real overwrites.
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
            [[file]]
            src = "LICENSE"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("Makefile.toml", "[tasks.run]\n")
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

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // Consumer edits both files.
    write(&pj.join("Makefile.toml"), "[tasks.run]\nedited = true\n");
    write(&pj.join("LICENSE"), "MIT\n# consumer addition\n");

    // status: `always` Makefile → `update` (drift), `once`
    // LICENSE → `skip(once)` (kata won't touch it).
    let assert = kata(td.path())
        .args(["status", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    // Line-paired so a token from one row can't satisfy the
    // other's assertion by accident (CodeRabbit nitpick).
    let has_update_makefile = stdout
        .lines()
        .any(|l| l.contains("update") && l.contains("Makefile.toml"));
    assert!(
        has_update_makefile,
        "always-mode edit must surface as update in status:\n{stdout}"
    );
    let has_skip_once_license = stdout
        .lines()
        .any(|l| l.contains("skip(once)") && l.contains("LICENSE"));
    assert!(
        has_skip_once_license,
        "once-mode edit must surface as skip(once), not update:\n{stdout}"
    );
    // Sanity: there must be no line that pairs `update` with
    // `LICENSE` — `update` LICENSE would mean kata is about to
    // overwrite the consumer's edit, which is exactly the
    // surprise we're preventing.
    let bad_update_license = stdout
        .lines()
        .any(|l| l.contains("update") && l.contains("LICENSE"));
    assert!(
        !bad_update_license,
        "status must not threaten to `update` a once file:\n{stdout}"
    );
}

#[test]
fn once_flag_composes_across_multiple_entries_to_same_dst() {
    // Issue #85: `when = "once"` must compose across multiple
    // `[[file]]` entries targeting the same dst. The flag is deferred
    // to a post-apply pass so the first entry's write doesn't lock
    // out the second entry's merge-toml on the same dst within the
    // same apply run.
    //
    // Fixture: pj-base overwrites `.kata/vars.toml` (when=once), then
    // pj-rust merge-toml's `actions.swatinem` into the same dst
    // (when=once). Before the fix this skipped the pj-rust entry
    // silently. After the fix both entries fire and the dst has
    // both keys.
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "vars.toml"
            dst = ".kata/vars.toml"
            how = "overwrite"
            when = "once"
            "#,
        )
        .file("vars.toml", "[actions]\ncheckout = \"v6\"\n");

    TemplateBuilder::new(templates.join("pj-rust"))
        .manifest(
            r#"
            name = "pj-rust"
            [[file]]
            src = "vars.rust.toml"
            dst = ".kata/vars.toml"
            how = "merge-toml"
            when = "once"
            paths = ["actions.swatinem"]
            "#,
        )
        .file("vars.rust.toml", "[actions]\nswatinem = \"v2\"\n");

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        [[templates]]
        source = "../templates/pj-rust"
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

    // After init, .kata/vars.toml must have BOTH keys merged.
    let vars_body = std::fs::read_to_string(pj.join(".kata/vars.toml")).unwrap();
    let vars: toml::Table = toml::from_str(&vars_body).unwrap();
    let actions = vars["actions"].as_table().expect("actions table missing");
    assert_eq!(
        actions.get("checkout").and_then(|v| v.as_str()),
        Some("v6"),
        "pj-base's `checkout` pin must be present: {vars_body}"
    );
    assert_eq!(
        actions.get("swatinem").and_then(|v| v.as_str()),
        Some("v2"),
        "pj-rust's `swatinem` pin (merge-toml from second layer) must be present: {vars_body}"
    );

    // applied.toml records once_applied = true for the composed dst
    let applied: toml::Table =
        toml::from_str(&std::fs::read_to_string(pj.join(".kata/applied.toml")).unwrap()).unwrap();
    let vars_state = applied["files"]
        .as_table()
        .unwrap()
        .get(".kata/vars.toml")
        .expect("vars.toml entry missing from applied.toml")
        .as_table()
        .unwrap();
    assert_eq!(
        vars_state.get("once_applied").and_then(|v| v.as_bool()),
        Some(true),
        "once_applied must be true after first apply"
    );

    // Consumer edits the file (Renovate-style bump or hand edit).
    let edit = "[actions]\ncheckout = \"v6.0.2\"\nswatinem = \"v2\"\nuser_added = \"true\"\n";
    write(&pj.join(".kata/vars.toml"), edit);

    // Second apply: both entries must skip (once_applied=true), so
    // the consumer's edit survives untouched.
    kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let after = std::fs::read_to_string(pj.join(".kata/vars.toml")).unwrap();
    assert!(
        after.contains("user_added"),
        "consumer's edit must survive second apply (once_applied gate): {after}"
    );
    assert!(
        after.contains("v6.0.2"),
        "Renovate-style bump must survive second apply: {after}"
    );
}

#[test]
fn reapply_reports_unchanged_for_layered_always_entries_to_same_dst() {
    // Issue #81: when two `when="always"` entries target the same dst
    // (e.g. pj-base overwrites renri.toml, pj-rust merge-toml's
    // `hooks.post_create` into it), a second apply that produces a
    // byte-identical disk result must report `unchanged` for both
    // entries — not `wrote`. Each layer individually still writes
    // intermediate bytes (overwrite clobbers, then merge-toml restores),
    // but the net disk delta across the entire apply is zero, so the
    // reporting should reflect "no observable change to disk".
    let td = TempDir::new().unwrap();
    let templates = td.path().join("templates");

    TemplateBuilder::new(templates.join("pj-base"))
        .manifest(
            r#"
            name = "pj-base"
            [[file]]
            src = "renri.toml.base"
            dst = "renri.toml"
            how = "overwrite"
            when = "always"
            "#,
        )
        .file("renri.toml.base", "[ui]\nshow_pr = true\n");

    TemplateBuilder::new(templates.join("pj-rust"))
        .manifest(
            r#"
            name = "pj-rust"
            [[file]]
            src = "renri.toml.rust"
            dst = "renri.toml"
            how = "merge-toml"
            when = "always"
            paths = ["hooks.post_create"]
            "#,
        )
        .file(
            "renri.toml.rust",
            "[[hooks.post_create]]\ntype = \"command\"\nrun = \"cargo make on-add\"\n",
        );

    let preset = write_preset(
        td.path(),
        "default",
        r#"
        name = "default"
        [[templates]]
        source = "../templates/pj-base"
        [[templates]]
        source = "../templates/pj-rust"
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

    let body = std::fs::read_to_string(pj.join("renri.toml")).unwrap();
    assert!(body.contains("show_pr = true"), "first apply: {body}");
    assert!(
        body.contains("post_create"),
        "first apply (merge-toml layer): {body}"
    );

    // Second apply: net disk delta is zero. The runner internally writes
    // twice (overwrite then merge-toml) but neither layer should be
    // reported as `wrote` to the user.
    let assertion = kata(td.path())
        .args(["apply", "--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assertion.get_output().stdout).to_string();

    // The two renri.toml lines from this dst must both be `unchanged`.
    let renri_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("renri.toml"))
        .collect();
    assert_eq!(
        renri_lines.len(),
        2,
        "expected one report line per layered entry, got:\n{stdout}"
    );
    for line in &renri_lines {
        assert!(
            line.contains("unchanged"),
            "layered always entries with zero net disk delta must report \
             `unchanged`, got: {line}\nfull stdout:\n{stdout}"
        );
    }
}
