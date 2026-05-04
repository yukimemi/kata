//! Phase 2-f end-to-end: `how = "script"` spawns the configured
//! command, with Tera-rendered args and `script_*` helper vars,
//! cwd'd into the project root.

use std::io::Write;
use std::path::{Path, PathBuf};

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

/// Cross-platform shell pair so a single test fixture works on
/// both Unix and Windows.
fn shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

#[test]
fn script_mode_runs_configured_command_and_writes_side_effect() {
    let td = TempDir::new().unwrap();
    let template_root = td.path().join("templates").join("scripts-demo");
    std::fs::create_dir_all(&template_root).unwrap();

    let (cmd, carg) = shell();

    write(
        &template_root.join("template.toml"),
        &format!(
            r#"
name = "scripts-demo"

[[file]]
src = "noop"
how = "script"
when = "always"
run = {{ command = "{cmd}", args = ["{carg}", "echo run-marker > marker.txt"] }}
"#
        ),
    );
    // `script` mode still goes through the runner's read-src step
    // (kata reads every src to a string before mode dispatch), so
    // an empty placeholder file has to exist.
    write(&template_root.join("noop"), "");

    let preset = write_preset(
        td.path(),
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
    let pj = td.path().join("demo");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    // Side-effect: the script ran with cwd = pj_root and wrote the
    // marker. Both the existence check (cwd was right) and the
    // content check (the command ran at all) matter.
    let marker = std::fs::read_to_string(pj.join("marker.txt"))
        .expect("script should have created marker.txt under pj_root");
    assert!(
        marker.contains("run-marker"),
        "marker should contain `run-marker`, got: {marker:?}"
    );
}

#[test]
fn script_mode_renders_tera_in_args_with_script_path_helper() {
    // Pin the `{{ script_path }}` / `{{ project.name }}` helpers
    // inside Tera-rendered run.args.
    let td = TempDir::new().unwrap();
    let template_root = td.path().join("templates").join("scripts-tera");
    std::fs::create_dir_all(&template_root).unwrap();

    let (cmd, carg) = shell();

    write(
        &template_root.join("template.toml"),
        &format!(
            r#"
name = "scripts-tera"

[[file]]
src = "payload.txt"
how = "script"
when = "always"
# {{{{ script_name }}}} resolves to the src filename ("payload.txt"),
# {{{{ project.name }}}} to the PJ name. Both should appear in the
# marker the script writes.
run = {{ command = "{cmd}", args = ["{carg}", "echo {{{{ script_name }}}}-{{{{ project.name }}}} > marker.txt"] }}
"#
        ),
    );
    write(&template_root.join("payload.txt"), "(unread)");

    let preset = write_preset(
        td.path(),
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
    let pj = td.path().join("my-pj");

    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .success();

    let marker = std::fs::read_to_string(pj.join("marker.txt"))
        .expect("script should have created marker.txt");
    assert!(
        marker.contains("payload.txt"),
        "marker should contain `script_name` (`payload.txt`): {marker:?}"
    );
    assert!(
        marker.contains("my-pj"),
        "marker should contain `project.name` (`my-pj`): {marker:?}"
    );
}

#[test]
fn script_mode_nonzero_exit_is_a_failed_outcome() {
    let td = TempDir::new().unwrap();
    let template_root = td.path().join("templates").join("failing");
    std::fs::create_dir_all(&template_root).unwrap();

    let (cmd, carg) = shell();

    write(
        &template_root.join("template.toml"),
        &format!(
            r#"
name = "failing"

[[file]]
src = "noop"
how = "script"
when = "always"
run = {{ command = "{cmd}", args = ["{carg}", "exit 7"] }}
"#
        ),
    );
    write(&template_root.join("noop"), "");

    let preset = write_preset(
        td.path(),
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
    let pj = td.path().join("demo");

    // kata wraps Failed file-outcomes into a non-zero CLI exit (the
    // existing apply_to_pj resilience policy). The test only needs
    // to confirm the failure surfaces, not the exact exit code.
    kata(td.path())
        .args(["init"])
        .arg(&preset)
        .args(["--at"])
        .arg(&pj)
        .arg("--non-interactive")
        .assert()
        .failure();
}
