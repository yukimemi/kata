# ROADMAP

Implementation plan for `kata`. See [CLAUDE.md](./CLAUDE.md) for the
settled design decisions; this file is the *how* and *in what order*.

## Phase 1 — MVP "single PJ + overwrite + once/always"

Smallest thing that proves the architecture. AI and parallelism
deferred. Makes kata equivalent to a tiny copier (vars-driven
overwrite templating with init-only files).

**Modules to land:**

- `cli.rs`, `cmd/{init,apply,status,list,doctor,completion}.rs`
- `manifest.rs`, `applied.rs`, `preset.rs` (Local source only —
  Git source is Phase 2)
- `template/source.rs` (Local + Path forms)
- `modes/overwrite.rs` (other `how`s = `unimplemented!`)
- `render/` (teravars integration + `kata.*` / `project.*` /
  `system.*` context)
- `runner/` skeleton — synchronous execution OK; tokio runtime
  exists but no fan-out yet
- `interactive.rs` — `inquire` prompts for vars
- Integration test: `tests/apply_basic.rs` (init → apply → status →
  diff → re-apply unchanged)

Done when: a Local fixture template can be `kata init`'d into a
fresh directory, vars get prompted (or `KATA_VAR_*`-injected),
files land, `.kata/applied.toml` exists, `kata apply` is a no-op.

## Phase 1 follow-ups (post-merge findings from dogfood)

The Phase 1 dogfood (kata applied to itself via local `pj-presets`)
surfaced two issues. Tracking here so they don't get lost between
phases.

- **Render opt-out marker** (Phase 2). Template files that legitimately
  contain `${{ ... }}` (GitHub Actions expressions, shell `${VAR:-x}`,
  `mustache`-using files, etc.) get mis-rendered as Tera variable
  references. Workaround today: wrap the file in
  `{% raw %}` ... `{% endraw %}` (Tera strips the markers on render).
  Proper fix: add a `render = false` field to `FileSpec`, and/or honour
  a `.notera` extension that skips rendering entirely. Decide on one
  approach and document.
- **`kata apply` resolution base** (fixed in Phase 1 follow-up PR).
  `applied.toml` now records `base_dir` so re-apply can resolve
  relative template `source` paths (`../pj-base`) against the original
  preset's directory rather than the PJ's cwd. The integration test
  `apply_resolves_template_sources_relative_to_recorded_base_dir`
  pins this behaviour.

## Phase 2 — "multi-template compose + git fetch + merge modes"

Resilience principle + structural mergers. After this phase kata
is genuinely useful for the user's existing 6 PJs (minus AI mode).

- `template/cache.rs` + `git.rs` — git clone / rev-parse / fetch
  (shell-out, yui style)
- `preset.rs::resolve` — fully resolves `<source>[@<rev>][//<subdir>][:<name>]`
  via git
- `modes/merge_section.rs` — marker-bracketed block replacement
- `modes/merge_toml.rs` — `toml_edit`, path-based merge
  (`paths = ["dependencies.renri"]`)
- `modes/merge_yaml.rs` — `serde_yaml`, same shape
- `modes/script.rs` — child process spawn
- `cmd/{add,remove,update}.rs`
- `tests/apply_modes.rs` — fixture per `how`
- Multi-template compose ordering test

## Phase 3 — "AI mode + tokio fan-out + progress UI"

Where kata gets its identity.

- `ai/{mod,claude,gemini,codex}.rs` — `AiAgent` trait + 3 backends
- `ai/prompt.rs` — diff + current + manifest prompt assembly
- `modes/ai.rs`
- `interactive.rs` — chezmoi-style `[a]ccept / [e]dit / [s]kip / [d]efer`
- `editor.rs` — `$EDITOR` integration
- `runner/` becomes tokio-native:
  - `JoinSet` for PJ-level fan-out
  - `Semaphore` per PJ for file parallelism
  - Global AI `Semaphore` (default 4)
  - `indicatif::MultiProgress`, one row per PJ
  - Per-PJ stdout buffer, flushed on PJ completion
- `cmd/pj.rs` — global registry add/remove/list
- `kata apply --all` — multi-PJ apply
- `--no-ai`, `--agent <kind>` flags
- `MockAiAgent` for deterministic tests

## Phase 4 — "completeness + dogfood + publish"

- `kata doctor` polished — detects `git`, `claude`, `gemini`,
  `codex`, `apm`, dead PJ paths in registry
- APM packaging — `.apm/skills/kata/SKILL.md` as source of truth
- `kata` itself uses `apm.yml` to install `yukimemi/renri#main`
- **Dogfood**: write `yukimemi/pj-presets:rust-cli` and apply it to
  `yukimemi/{shun, rvpm, todoke, yui, renri, spyrun, kata}`
- README + ROADMAP polish for v0.1.0
- `tests/multi_pj.rs` with 3 real tempdir PJs in parallel

---

## Crate structure

```
kata/
├── Cargo.toml                      # bin "kata" + lib "kata"
├── Cargo.lock
├── Makefile.toml                   # check / fmt / clippy / test / setup
├── apm.yml                         # imports renri (worktree workflow)
├── apm.lock.yaml
├── CLAUDE.md
├── ROADMAP.md
├── README.md
├── LICENSE
├── renri.toml                      # dev-time worktree config
├── renovate.json
├── .apm/skills/kata/SKILL.md       # kata AI mode usage guide
├── src/
│   ├── main.rs                     # Cli::parse → tokio runtime → Cli::run
│   ├── lib.rs                      # mod list + tracing init + Error/Result
│   ├── cli.rs                      # clap (Cli, Command, subcommands)
│   ├── cmd/
│   │   ├── mod.rs                  # dispatch
│   │   ├── init.rs                 # kata init <preset>
│   │   ├── apply.rs                # kata apply (default)
│   │   ├── status.rs
│   │   ├── diff.rs
│   │   ├── add.rs                  # kata add <template>
│   │   ├── remove.rs
│   │   ├── list.rs
│   │   ├── doctor.rs
│   │   ├── pj.rs                   # global registry add/remove/list
│   │   ├── update.rs               # kata update — refetch templates
│   │   └── completion.rs
│   ├── error.rs                    # thiserror::Error
│   ├── icons.rs                    # Unicode / Nerd / Ascii (yui style)
│   ├── config/
│   │   ├── mod.rs                  # GlobalConfig
│   │   └── registry.rs             # [[project]] r/w via toml_edit
│   ├── manifest.rs                 # template.toml schema
│   ├── preset.rs                   # preset.toml schema + resolve()
│   ├── applied.rs                  # .kata/applied.toml r/w
│   ├── template/
│   │   ├── mod.rs                  # TemplateHandle + file enumeration
│   │   ├── source.rs               # TemplateSource enum (Git/Local/Path)
│   │   └── cache.rs                # ~/.cache/kata/templates/<src-hash>@<rev>
│   ├── render/
│   │   ├── mod.rs                  # teravars::Engine wrapper
│   │   ├── context.rs              # kata.* / vars.* / system.* / project.*
│   │   └── vars.rs                 # vars resolution (prec order)
│   ├── modes/
│   │   ├── mod.rs                  # ApplyMode trait + dispatch
│   │   ├── overwrite.rs
│   │   ├── merge_section.rs        # marker-bracketed
│   │   ├── merge_toml.rs           # toml_edit + path merge
│   │   ├── merge_yaml.rs
│   │   ├── ai.rs
│   │   └── script.rs
│   ├── ai/
│   │   ├── mod.rs                  # AiAgent trait + AgentKind + auto fallback
│   │   ├── claude.rs
│   │   ├── gemini.rs
│   │   ├── codex.rs
│   │   └── prompt.rs               # diff + current + prompt assembly
│   ├── git.rs                      # shell-out git
│   ├── runner/
│   │   ├── mod.rs                  # tokio fan-out
│   │   ├── plan.rs                 # ApplyPlan { actions: Vec<Action> }
│   │   ├── action.rs               # 1 file × 1 mode = 1 action
│   │   ├── progress.rs             # indicatif MultiProgress + buffer
│   │   └── outcome.rs              # ActionOutcome / PjOutcome / ApplyReport
│   ├── ui/
│   │   ├── mod.rs                  # output formatting (yui-style)
│   │   ├── prompt.rs               # inquire wrapper
│   │   └── diff.rs                 # similar crate
│   ├── interactive.rs              # a/e/s/d for AI results
│   ├── paths.rs                    # config / cache / pj root resolution
│   └── editor.rs                   # $EDITOR for [e]dit
├── tests/
│   ├── cli.rs
│   ├── apply_basic.rs
│   ├── apply_modes.rs
│   ├── apply_drift.rs
│   ├── multi_pj.rs
│   ├── ai_mock.rs
│   └── fixtures/
│       ├── presets/rust-cli.toml
│       ├── templates/pj-base/
│       ├── templates/pj-rust/
│       └── templates/pj-rust-cli/
└── .github/workflows/              # CI (renri / yui shape)
```

### Why `cmd/` is split into per-subcommand files

yui's `src/cmd.rs` grew to **6347 lines** and became hard to
navigate. kata starts with one file per subcommand from day one.
The dispatch table lives in `cmd/mod.rs`.

### Why `modes/` and `ai/` are separate

`how` (the *method* of applying a file) and the AI backend (the
*how* of the AI call itself) are independent axes. `ApplyMode` and
`AiAgent` are two distinct traits in two distinct modules. AI mode
is one of the `how`s — it composes `AiAgent` rather than wrapping it.

### Why `runner/` is its own module

tokio fan-out, semaphores, progress, and per-PJ output buffering
are cross-cutting concerns. Each `cmd` builds an `ApplyPlan` and
hands it to `runner::execute` — no `cmd` ever spawns tasks
directly.

---

## Key types and traits (signatures)

### Error

```rust
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("config: {0}")] Config(String),
    #[error("manifest: {0}")] Manifest(String),
    #[error("preset: {0}")] Preset(String),
    #[error("applied state: {0}")] Applied(String),
    #[error("template: {0}")] Template(String),
    #[error("git: {0}")] Git(String),
    #[error("merge: {0}")] Merge(String),
    #[error("ai backend `{agent}` not available: {reason}")]
    AiUnavailable { agent: String, reason: String },
    #[error("ai backend `{agent}` failed (exit={code:?}): {stderr}")]
    AiFailed { agent: String, code: Option<i32>, stderr: String },
    #[error("project not registered: {0}")] PjUnknown(String),
    #[error("user cancelled")] Cancelled,
    #[error(transparent)] Tera(#[from] teravars::Error),
    #[error(transparent)] Other(#[from] anyhow::Error),
}
```

### GlobalConfig

```rust
#[derive(Debug, Deserialize, Serialize, Default)]
pub struct GlobalConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "project")]
    pub projects: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct Defaults {
    #[serde(default)]
    pub default_agent: AgentKind,
    #[serde(default = "default_ai_concurrency")]
    pub ai_concurrency: usize,        // default 4
    #[serde(default)]
    pub pj_concurrency: Option<usize>,// default num_cpus or 8
    #[serde(default)]
    pub icons: IconsMode,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProjectEntry {
    pub name: String,
    pub path: Utf8PathBuf,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub overrides: Option<ProjectOverrides>,
}
```

### Preset

```rust
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Preset {
    pub name: String,
    pub templates: Vec<TemplateRef>,    // compose order, last wins
    #[serde(default)]
    pub vars: toml::Table,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TemplateRef {
    pub source: String,                 // url or local path
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default)]
    pub subdir: Option<String>,
}

impl Preset {
    pub async fn resolve(spec: &str, cache: &TemplateCache) -> Result<Self>;
}
```

### Manifest

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default, rename = "file")]
    pub files: Vec<FileSpec>,
    #[serde(default)]
    pub vars: BTreeMap<String, VarSpec>,
    #[serde(default)]
    pub requires: Requires,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FileSpec {
    pub src: String,                    // glob OK
    #[serde(default)]
    pub dst: Option<String>,            // Tera OK
    pub how: HowMode,
    #[serde(default)]
    pub when: WhenMode,
    #[serde(default)]
    pub agent: Option<AgentKind>,
    #[serde(default)]
    pub prompt: Option<String>,         // Tera OK
    #[serde(default)]
    pub marker: Option<MarkerSpec>,     // for merge-section
    #[serde(default)]
    pub paths: Vec<String>,             // for merge-toml/yaml
    #[serde(default)]
    pub when_expr: Option<String>,      // Tera bool
    #[serde(default)]
    pub run: Option<ScriptSpec>,        // for script
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HowMode {
    Overwrite, MergeSection, MergeToml, MergeYaml, Ai, Script,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WhenMode { Once, #[default] Always, Manual }
```

### AppliedState (`.kata/applied.toml`)

```rust
#[derive(Debug, Deserialize, Serialize, Default)]
pub struct AppliedState {
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub templates: Vec<AppliedTemplate>,// compose order
    #[serde(default)]
    pub applied_at: Option<jiff::Timestamp>,
    #[serde(default)]
    pub vars: toml::Table,
    #[serde(default)]
    pub files: BTreeMap<String, FileState>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct FileState {
    #[serde(default)]
    pub last_ai_run: Option<jiff::Timestamp>,
    #[serde(default)]
    pub last_decision: Option<Decision>,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub once_applied: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision { Accept, Edit, Skip, Defer }
```

### AiAgent

```rust
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind { #[default] Auto, Claude, Gemini, Codex }

#[derive(Debug, Clone)]
pub struct AiRequest {
    pub system_prompt: String,
    pub user_prompt: String,
    pub current: Option<String>,
    pub incoming: String,
    pub template_diff: Option<String>,
    pub dst: Utf8PathBuf,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AiResponse {
    pub full_body: Option<String>,
    pub patch: Option<String>,          // exclusive with full_body
    pub raw: String,
    pub agent: AgentKind,
}

#[async_trait::async_trait]
pub trait AiAgent: Send + Sync {
    fn kind(&self) -> AgentKind;
    async fn is_available(&self) -> bool;
    async fn run(&self, req: AiRequest) -> Result<AiResponse>;
}

pub async fn resolve_agent(
    requested: AgentKind,
    cfg: &Defaults,
) -> Result<Box<dyn AiAgent>>;
```

### ApplyMode

```rust
#[async_trait::async_trait]
pub trait ApplyMode: Send + Sync {
    async fn plan(&self, ctx: &ActionContext<'_>) -> Result<ActionPlan>;
    async fn execute(&self, ctx: &ActionContext<'_>, dry_run: bool)
        -> Result<ActionOutcome>;
}

pub struct ActionContext<'a> {
    pub project: &'a ProjectEntry,
    pub pj_root: &'a Utf8Path,
    pub template: &'a TemplateHandle,
    pub spec: &'a FileSpec,
    pub src_abs: Utf8PathBuf,
    pub dst_abs: Utf8PathBuf,
    pub rendered_body: String,
    pub current_body: Option<String>,
    pub vars: &'a toml::Table,
    pub tera_ctx: &'a tera::Context,
    pub agent: Option<Arc<dyn AiAgent>>,
    pub interactive: bool,
}
```

### Runner

```rust
pub struct ApplyPlan {
    pub projects: Vec<PjPlan>,
}

pub struct PjPlan {
    pub project: ProjectEntry,
    pub pj_root: Utf8PathBuf,
    pub vars: toml::Table,
    pub actions: Vec<ActionItem>,
    pub templates: Vec<AppliedTemplate>,
}

pub struct RunnerOpts {
    pub dry_run: bool,
    pub no_ai: bool,
    pub agent_override: Option<AgentKind>,
    pub pj_concurrency: usize,
    pub ai_concurrency: usize,
    pub interactive: bool,
    pub icons: Icons,
}

pub async fn execute(plan: ApplyPlan, opts: RunnerOpts) -> Result<ApplyReport>;
```

---

## CLI surface

| subcommand | flags | behavior |
|---|---|---|
| `kata init <preset>` | `--at <path>` (default cwd), `--non-interactive`, `--from-applied <path>`, `--register / --no-register` | Resolve preset → prompt vars → apply all templates (`once`+`always`) → write `.kata/applied.toml` → register in global config |
| `kata apply [<pj>...]` | `--dry-run`, `--no-ai`, `--agent <kind>`, `--file <path>`, `--tag <tag>`, `--all`, `--only-template <name>`, `--jobs <N>`, `--non-interactive`, `--yes`, `--var name=val` | Default verb. No args = cwd PJ; with names/paths = registry lookup; `--all` = every registered PJ; `--file` runs single-file `manual` mode |
| `kata status [<pj>...]` | `--all`, `--icons`, `--no-color`, `--json` | Show drift / pending. Read-only |
| `kata diff [<pj>...]` | `--all`, `--file <path>`, `--no-color` | Unified diff, yui-style |
| `kata add <template>` | `--rev <ref>`, `--at <path>` | Append to `applied.toml.templates` then `apply` |
| `kata remove <template>` | `--at <path>`, `--clean` | Remove from `applied.toml`. `--clean` also deletes files (confirms first) |
| `kata list` | `--templates`, `--projects`, `--preset <preset>`, `--icons` | Inventory views |
| `kata pj add <path>` | `--name <name>`, `--tags <a,b>` | Register a PJ in global config |
| `kata pj remove <name-or-path>` | | Deregister |
| `kata pj list` | `--icons`, `--no-color` | Show registry |
| `kata update [<template>...]` | `--all`, `--rev <ref>`, `--apply` | Re-fetch template repos and update `applied.toml` revs |
| `kata doctor` | `--icons`, `--no-color` | Check `git`, `claude`, `gemini`, `codex`, `apm`, dead PJ paths |
| `kata completion <shell>` | | bash / zsh / fish / pwsh |

Global flags: `--config / -C`, `--verbose / -v`, `--non-interactive`,
`--no-color`, `--icons`.

---

## Testing approach

### Unit tests (per-module `#[cfg(test)]`)

- `manifest.rs` — schema deserialize, Error on bad values
- `applied.rs` — round-trip, `promote_template` last-wins, `record`
- `preset.rs` — spec parsing (`github.com/x/y:rust-cli`,
  `git+ssh://...:branch`)
- `render/context.rs` — Tera context building
- `modes/merge_toml.rs` — path-based merge cases (new key, override,
  array)
- `modes/merge_section.rs` — marker detection, append on miss,
  duplicate marker handling
- `ai/prompt.rs` — golden-compare prompt assembly

### Integration tests (`tests/`, `assert_cmd`)

```rust
#[test]
fn init_then_apply_writes_files() {
    let td = TempDir::new().unwrap();
    let tpl = setup_template_repo(&td);
    let pj = setup_pj(&td);

    Command::cargo_bin("kata").unwrap()
        .args(["init", &tpl.to_string()])
        .arg("--at").arg(&pj)
        .arg("--non-interactive")
        .env("KATA_VAR_project", "demo")
        .env("KATA_HOME", td.path().join("kata"))
        .assert().success();

    assert!(pj.join("Makefile.toml").exists());
    assert!(pj.join(".kata/applied.toml").exists());
}
```

Strategy:
- `KATA_HOME` env var overrides global config dir for isolation
- `KATA_VAR_<name>` injects vars without prompting
- Local-form templates dominate fixtures; one git test uses
  `git init --bare` tempdir as remote
- `MockAiAgent` for deterministic AI tests, injected via runner

### Parallel test stability

- Default to `--jobs 1` for most tests
- Multi-PJ test uses `--jobs 4` with order-independent assertions
- AI semaphore test uses `tokio::sync::Notify` to count concurrent
  permits

---

## Open questions

Settled before implementation:

- ✅ **Q1 — preset spec grammar:** `<source>[@<rev>][//<subdir>][:<preset-name>]`
- ✅ **Q2 — vars precedence:** CLI > env > applied > preset > default > prompt

Defer to phase boundaries:

- **Q3 — git clone strategy:** shell-out `git clone --depth 1`
  (decision: shell-out, libgit2 is too painful on Windows)
- **Q4 — cache GC:** TTL-less, refetch every time, `kata cache gc`
  added in Phase 4 only if needed
- **Q5 — vars name collision across templates:** last-wins by
  default; type mismatch is an error. Template authors are advised
  to use namespaced var names (`{prefix}_xxx`)
- **Q6 — `once` mode "applied" check:** rely solely on
  `applied.toml.files["<dst>"].once_applied`. Don't second-guess
  the truth file
- **Q7 — `[e]dit` semantics:** open the AI's proposed body in
  `$EDITOR`; on save, accept as-is. Iterate-with-AI flow is Phase 4+
- **Q8 — `merge-section` markers:** **required** in manifest, no
  inference from file extension. Explicit > implicit
- **Q9 — PJ registration safety:** `init` searches upward for
  `.kata/`; refuse if found in an ancestor. Registry duplicates by
  name = error; by path = no-op
- **Q10 — secret vars:** `secret = true` in `VarSpec` enables
  inquire `Password` (echo-suppressed). No vault integration in MVP
- **Q11 — log ordering within a PJ:** preserve `manifest.files`
  order in output even when execution is parallel
- **Q12 — `--non-interactive` + AI:** default skip; `--yes` accepts
  everything. `--non-interactive --yes` = full CI AI mode
- **Q13 — manifest version breaking-change detection:** optional
  field; detection deferred to Phase 4 `kata update` polish

---

## Reference projects (design inputs)

- **yui** (`yukimemi/yui`) — `apply` / `status` / `diff` / `dry-run`
  vocabulary, `--icons` / `--non-interactive` flags, resilience
  philosophy. Reaction: split `cmd.rs` from day one.
- **renri** (`yukimemi/renri`) — typed hook executor pattern,
  worktree-first dev workflow, APM packaging
- **teravars** (`yukimemi/teravars`) — vars + Tera engine + include
  + system context. Used directly, not reimplemented
- **rvpm** (`yukimemi/rvpm`) — resilience principle, parallel
  execution patterns
- **copier** (Python) — closest existing tool conceptually, the
  reference for what "init + update" should feel like
