//! clap CLI surface.

use camino::Utf8PathBuf;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::cmd;
use crate::error::Result;
use crate::manifest::{AgentKind, AiMode};

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

/// `--ai-mode <chat|handoff>` choices. Maps directly onto
/// `manifest::AiMode` but stays a separate clap enum so the help
/// text describes the *run-wide override* semantics specifically.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AiModeArg {
    /// Run kata's chezmoi-style chat dialog (default).
    Chat,
    /// Skip the chat loop and spawn the agent CLI interactively for
    /// every `how = "ai"` file. kata stops re-importing.
    Handoff,
}

impl From<AiModeArg> for AiMode {
    fn from(a: AiModeArg) -> Self {
        match a {
            AiModeArg::Chat => AiMode::Chat,
            AiModeArg::Handoff => AiMode::Handoff,
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
        /// Free-form instruction prepended to every `how = "ai"`
        /// request for this run (e.g. "respond in Japanese", "always
        /// keep my custom Section X"). Stacks on top of the per-file
        /// `prompt` from the manifest.
        #[arg(long = "ai-prompt", value_name = "MSG")]
        ai_prompt: Option<String>,
        /// Run-wide override for the per-file `ai_mode`. `handoff`
        /// makes every `how = "ai"` file go straight to the agent
        /// CLI (kata stops re-importing); omit to honour each
        /// manifest's declared mode (default `chat`).
        #[arg(long = "ai-mode", value_enum, value_name = "MODE")]
        ai_mode: Option<AiModeArg>,
        /// Maximum concurrent AI calls (chat turns / handoff
        /// spawns / editor round-trips). Overrides
        /// `defaults.ai_concurrency` (default 4) for this run.
        #[arg(long = "ai-concurrency", value_name = "N")]
        ai_concurrency: Option<usize>,
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
        /// Free-form instruction prepended to every `how = "ai"`
        /// request for this run (e.g. "respond in Japanese", "always
        /// keep my custom Section X"). Stacks on top of the per-file
        /// `prompt` from the manifest.
        #[arg(long = "ai-prompt", value_name = "MSG")]
        ai_prompt: Option<String>,
        /// Run-wide override for the per-file `ai_mode`. `handoff`
        /// makes every `how = "ai"` file go straight to the agent
        /// CLI (kata stops re-importing); omit to honour each
        /// manifest's declared mode (default `chat`).
        #[arg(long = "ai-mode", value_enum, value_name = "MODE")]
        ai_mode: Option<AiModeArg>,
        /// Maximum concurrent AI calls (chat turns / handoff
        /// spawns / editor round-trips). Overrides
        /// `defaults.ai_concurrency` (default 4) for this run.
        #[arg(long = "ai-concurrency", value_name = "N")]
        ai_concurrency: Option<usize>,
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
        /// Free-form instruction prepended to every `how = "ai"`
        /// request for this run (e.g. "respond in Japanese", "always
        /// keep my custom Section X"). Stacks on top of the per-file
        /// `prompt` from the manifest.
        #[arg(long = "ai-prompt", value_name = "MSG")]
        ai_prompt: Option<String>,
        /// Run-wide override for the per-file `ai_mode`. `handoff`
        /// makes every `how = "ai"` file go straight to the agent
        /// CLI (kata stops re-importing); omit to honour each
        /// manifest's declared mode (default `chat`).
        #[arg(long = "ai-mode", value_enum, value_name = "MODE")]
        ai_mode: Option<AiModeArg>,
        /// Maximum concurrent AI calls (chat turns / handoff
        /// spawns / editor round-trips). Overrides
        /// `defaults.ai_concurrency` (default 4) for this run.
        #[arg(long = "ai-concurrency", value_name = "N")]
        ai_concurrency: Option<usize>,
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

    /// List inventory. Without `--all`, prints what governs the
    /// current PJ. With `--all`, walks the global registry and
    /// shows a one-row-per-PJ overview (preset / templates /
    /// last-applied / status).
    List {
        #[arg(long, value_name = "DIR")]
        at: Option<Utf8PathBuf>,
        /// Show every PJ from the global registry instead of only
        /// the current one.
        #[arg(long)]
        all: bool,
    },

    /// Add a project to the global registry
    /// (`~/.config/kata/config.toml`). Optional `--name` defaults
    /// to the upstream repo basename; `--tags` are repeatable
    /// labels for filtering with `kata apply --all --tag <t>`
    /// (Phase 5-b).
    Register {
        #[arg(value_name = "PATH")]
        path: Option<Utf8PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },

    /// Drop a project from the global registry. The PJ's
    /// `.kata/applied.toml` is left alone — only the registry
    /// pointer goes away.
    Unregister {
        /// Project name (from registry) or absolute path.
        key: String,
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
                ai_prompt,
                ai_mode,
                ai_concurrency,
            } => {
                let (kind, no_ai) = resolve_ai_inputs(ai, no_ai);
                cmd::init::run(
                    preset,
                    at,
                    vars,
                    kind,
                    no_ai,
                    yes,
                    ai_prompt,
                    ai_mode.map(Into::into),
                    ai_concurrency,
                    interactive,
                    no_color,
                )
                .await
            }
            Command::Apply {
                at,
                dry_run,
                vars,
                ai,
                no_ai,
                yes,
                ai_prompt,
                ai_mode,
                ai_concurrency,
            } => {
                let (kind, no_ai) = resolve_ai_inputs(ai, no_ai);
                cmd::apply::run(
                    at,
                    dry_run,
                    vars,
                    kind,
                    no_ai,
                    yes,
                    ai_prompt,
                    ai_mode.map(Into::into),
                    ai_concurrency,
                    interactive,
                    no_color,
                )
                .await
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
                ai_prompt,
                ai_mode,
                ai_concurrency,
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
                    ai_prompt,
                    ai_mode.map(Into::into),
                    ai_concurrency,
                    interactive,
                    no_color,
                )
                .await
            }
            Command::Remove { template, at } => cmd::remove::run(template, at, no_color).await,
            Command::Update { templates, rev, at } => {
                cmd::update::run(templates, rev, at, no_color).await
            }
            Command::List { at, all } => cmd::list::run(at, all, no_color),
            Command::Register { path, name, tags } => {
                cmd::register::run(path, name, tags, no_color).await
            }
            Command::Unregister { key } => cmd::unregister::run(key, no_color),
            Command::Doctor => cmd::doctor::run(no_color),
            Command::Completion { shell } => {
                let mut c = Cli::command();
                clap_complete::generate(shell, &mut c, "kata", &mut std::io::stdout());
                Ok(())
            }
        }
    }
}
