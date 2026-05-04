//! clap CLI surface.

use camino::Utf8PathBuf;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::cmd;
use crate::error::Result;
use crate::manifest::AgentKind;

/// `--ai <BACKEND>` choices, including the `off` shortcut for
/// `--no-ai`. Stays separate from `manifest::AgentKind` because
/// `off` is a CLI-only state (the manifest can't request "no AI").
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AiBackendArg {
    /// Pick the first installed CLI in the order claude > codex >
    /// gemini (default).
    Auto,
    Claude,
    Gemini,
    Codex,
    /// Skip every `how = "ai"` file. Equivalent to `--no-ai`.
    Off,
}

impl AiBackendArg {
    /// Translate the CLI choice into the (`AgentKind`, `no_ai`)
    /// pair the runner expects. `Off` becomes "no agent + no_ai".
    pub fn into_runner_inputs(self) -> (AgentKind, bool) {
        match self {
            AiBackendArg::Auto => (AgentKind::Auto, false),
            AiBackendArg::Claude => (AgentKind::Claude, false),
            AiBackendArg::Gemini => (AgentKind::Gemini, false),
            AiBackendArg::Codex => (AgentKind::Codex, false),
            AiBackendArg::Off => (AgentKind::Auto, true),
        }
    }
}

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
        /// AI backend for `how = "ai"` files (auto / claude / gemini / codex / off).
        #[arg(long, value_enum, default_value_t = AiBackendArg::Auto)]
        ai: AiBackendArg,
        /// Skip every `how = "ai"` file. Equivalent to `--ai off`.
        #[arg(long, conflicts_with = "ai")]
        no_ai: bool,
        /// Accept AI-generated bodies non-interactively.
        #[arg(long)]
        yes: bool,
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
        /// AI backend for `how = "ai"` files (auto / claude / gemini / codex / off).
        #[arg(long, value_enum, default_value_t = AiBackendArg::Auto)]
        ai: AiBackendArg,
        /// Skip every `how = "ai"` file. Equivalent to `--ai off`.
        #[arg(long, conflicts_with = "ai")]
        no_ai: bool,
        /// Accept AI-generated bodies non-interactively.
        #[arg(long)]
        yes: bool,
    },

    /// Show what would change if `apply` were to run.
    Status {
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
    },

    /// Append a template to this project's applied state and apply.
    Add {
        /// Template spec: `<source>[@<rev>][//<subdir>]`. Same
        /// grammar as preset templates.
        template: String,
        /// Pin the new template at this rev (branch / tag / SHA).
        #[arg(long)]
        rev: Option<String>,
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
        #[arg(long = "var", value_name = "NAME=VAL")]
        vars: Vec<String>,
        /// AI backend for `how = "ai"` files (auto / claude / gemini / codex / off).
        #[arg(long, value_enum, default_value_t = AiBackendArg::Auto)]
        ai: AiBackendArg,
        /// Skip every `how = "ai"` file. Equivalent to `--ai off`.
        #[arg(long, conflicts_with = "ai")]
        no_ai: bool,
        /// Accept AI-generated bodies non-interactively.
        #[arg(long)]
        yes: bool,
    },

    /// Drop a template from this project's applied state.
    Remove {
        /// Template name or full source spec. Tail-segment match
        /// also works (e.g. `kata remove pj-rust` for
        /// `github.com/yukimemi/pj-rust`).
        template: String,
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
    },

    /// Refresh the cache slot for git-sourced templates and bump
    /// recorded revs in `applied.toml`. No-op for local templates.
    Update {
        /// Templates to update (name or full source). Empty = all.
        templates: Vec<String>,
        /// Override the rev to check out (default = HEAD of
        /// upstream's default branch).
        #[arg(long)]
        rev: Option<String>,
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

/// Fold the `--ai <kind>` choice and the `--no-ai` shortcut into
/// the `(AgentKind, no_ai)` pair `cmd::*::run` consumes. `--no-ai`
/// always wins over `--ai`; clap's `conflicts_with` already keeps
/// them from coexisting at parse time, but the helper stays
/// defensive in case a programmatic caller bypasses that.
fn resolve_ai_inputs(ai: AiBackendArg, no_ai: bool) -> (AgentKind, bool) {
    let (kind, off) = ai.into_runner_inputs();
    (kind, off || no_ai)
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        let interactive = !self.non_interactive;
        let no_color = self.no_color;
        match self.command {
            Command::Init {
                preset,
                at,
                vars,
                ai,
                no_ai,
                yes,
            } => {
                let (kind, no_ai) = resolve_ai_inputs(ai, no_ai);
                cmd::init::run(preset, at, vars, kind, no_ai, yes, interactive, no_color).await
            }
            Command::Apply {
                at,
                dry_run,
                vars,
                ai,
                no_ai,
                yes,
            } => {
                let (kind, no_ai) = resolve_ai_inputs(ai, no_ai);
                cmd::apply::run(at, dry_run, vars, kind, no_ai, yes, interactive, no_color).await
            }
            Command::Status { at } => cmd::status::run(at, interactive, no_color).await,
            Command::Add {
                template,
                rev,
                at,
                vars,
                ai,
                no_ai,
                yes,
            } => {
                let (kind, no_ai) = resolve_ai_inputs(ai, no_ai);
                cmd::add::run(
                    template,
                    rev,
                    at,
                    vars,
                    kind,
                    no_ai,
                    yes,
                    interactive,
                    no_color,
                )
                .await
            }
            Command::Remove { template, at } => cmd::remove::run(template, at, no_color).await,
            Command::Update { templates, rev, at } => {
                cmd::update::run(templates, rev, at, no_color).await
            }
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
