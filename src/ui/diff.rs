//! Diff colouring. The unified-diff string itself is built by the
//! mode (`overwrite::unified_diff`); this module just adds ANSI
//! colour to the output line-by-line.

use owo_colors::OwoColorize;

use super::color_enabled;

pub fn print_diff(diff: &str, no_color: bool) {
    if !color_enabled(no_color) {
        print!("{diff}");
        return;
    }
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            println!("{}", line.bold());
        } else if line.starts_with('+') {
            println!("{}", line.green());
        } else if line.starts_with('-') {
            println!("{}", line.red());
        } else if line.starts_with("@@") {
            println!("{}", line.cyan());
        } else {
            println!("{line}");
        }
    }
}
