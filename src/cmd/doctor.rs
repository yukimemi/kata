//! `kata doctor` — environment sanity check.

use crate::error::Result;
use crate::paths::{global_config_dir, template_cache_dir};
use crate::ui;

use super::doctor_helpers::detect;

pub fn run(no_color: bool) -> Result<()> {
    let _ = no_color; // Phase 1 doesn't colour doctor output yet
    println!(
        "kata {} on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    println!();

    println!("Tooling:");
    pair("git", detect("git", &["--version"]));
    pair("claude", detect("claude", &["--version"]));
    pair("gemini", detect("gemini", &["--version"]));
    pair("codex", detect("codex", &["--version"]));
    pair("apm", detect("apm", &["--version"]));
    println!();

    println!("Paths:");
    match global_config_dir() {
        Ok(p) => println!("  global config: {p}"),
        Err(e) => println!("  global config: <error: {e}>"),
    }
    match template_cache_dir() {
        Ok(p) => println!("  cache:         {p}"),
        Err(e) => println!("  cache:         <error: {e}>"),
    }
    let _ = ui::color_enabled(no_color); // touch UI so the import isn't unused
    Ok(())
}

fn pair(name: &str, ok: bool) {
    let mark = if ok { "✓" } else { "✗" };
    println!("  [{mark}] {name}");
}
