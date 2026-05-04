//! clap CLI surface.

use camino::Utf8PathBuf;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use crate::cmd;
use crate::error::Result;

/// Help-text styling — mirrored from yui so all yukimemi CLIs feel
/// like the same family.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Magenta.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

#[derive(Parser, Debug)]
#[command(version, about, long_about = None, styles = HELP_STYLES)]
pub struct Cli {
    /// Increase log verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Disable color output (also respected via NO_COLOR env).
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Refuse to prompt; missing values become errors.
    #[arg(long, global = true)]
    pub non_interactive: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Bootstrap a new project from a preset (Phase 1 supports
    /// local presets only).
    Init {
        /// Preset spec: `<source>[@<rev>][//<subdir>][:<preset-name>]`.
        /// For Phase 1 use a local path or a path to a `.toml` file.
        preset: String,
        /// Project root (defaults to cwd).
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
        /// `--var name=value` (repeatable). Highest precedence.
        #[arg(long = "var", value_name = "NAME=VAL")]
        vars: Vec<String>,
    },

    /// Re-apply this project's templates against the recorded state.
    Apply {
        /// Project root (defaults to cwd, walking upwards to find
        /// `.kata/applied.toml`).
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
        /// Preview only; no files written, no state updated.
        #[arg(long)]
        dry_run: bool,
        /// `--var name=value` (repeatable).
        #[arg(long = "var", value_name = "NAME=VAL")]
        vars: Vec<String>,
    },

    /// Show what would change if `apply` were to run.
    Status {
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
    },

    /// List inventory (registered projects / template files in this
    /// PJ).
    List {
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
    },

    /// Diagnose environment (git, agent CLIs, config dirs).
    Doctor,

    /// Print shell completion script.
    Completion {
        /// bash | zsh | fish | powershell | elvish
        shell: Shell,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        let interactive = !self.non_interactive;
        let no_color = self.no_color;
        match self.command {
            Command::Init { preset, at, vars } => {
                cmd::init::run(preset, at, vars, interactive, no_color).await
            }
            Command::Apply { at, dry_run, vars } => {
                cmd::apply::run(at, dry_run, vars, interactive, no_color).await
            }
            Command::Status { at } => cmd::status::run(at, interactive, no_color).await,
            Command::List { at } => cmd::list::run(at, no_color),
            Command::Doctor => cmd::doctor::run(no_color),
            Command::Completion { shell } => {
                let mut c = Cli::command();
                clap_complete::generate(shell, &mut c, "kata", &mut std::io::stdout());
                Ok(())
            }
        }
    }
}
