# AGENTS.md

Guidance for AI coding agents (Claude Code, Gemini CLI, OpenAI
Codex, etc.) working in this repo. Each agent's preferred entry
point (`CLAUDE.md`, `GEMINI.md`, …) is a thin shim that points
back here, so this file is the single source of truth.

## What kata is

A meta-templating CLI for **multi-project scaffold + continuous
sync**. Apply layered templates (`pj-base` + `pj-rust` +
`pj-rust-cli`, …) to many sibling projects (Rust / Go / Bun) and
keep them in sync as the templates evolve. Conceptually a Rust
copier-equivalent, with a novelty: AI delegation mode for files
that resist mechanical merge (AGENTS.md / ROADMAP.md / README.md).

Name comes from 型 (kata) — the woodblock / stencil / pattern that
gets pressed onto each project. Multiple kata layered = compose. The
AI mode is "applying the kata, then asking a calligrapher to adapt".
Crate name `kata`, binary `kata`, repo `yukimemi/kata`.

## Companion projects (the reason this exists)

`kata` exists because the author maintains a growing set of sibling
Rust CLIs (yukimemi/{shun, rvpm, todoke, yui, renri, spyrun}) that
share most of their `Makefile.toml`, `apm.yml`, `renri.toml`, CI,
and `AGENTS.md` boilerplate. Updating the boilerplate by
hand-copy-paste across N projects is the pain point being fixed.

The same toolchain should apply to Go and Bun siblings — the
language-specific bits live in their own template repos
(`pj-rust`, `pj-go`, `pj-bun`), and a `pj-base` carries the
language-agnostic boilerplate (LICENSE, .gitignore basics, common
AGENTS.md sections).

## File layers

There are four distinct config files in the kata system, each owned
by a different stakeholder. **Don't conflate them** — that was the
single biggest design risk discussed up front.

| Layer | Owner | Path | Role |
|---|---|---|---|
| 0. global config | the user | `~/.config/kata/config.toml` | Tool defaults (`default_agent`, `ai_concurrency`) + `[[project]]` registry of where the user's PJs live |
| 1. preset | template author | external git repo, e.g. `yukimemi/pj-presets/rust-cli.toml` | A *bundle* — names which templates compose together, in what order |
| 2. template manifest | template author | each template repo's `template.toml` | `vars` definitions + per-file `how` × `when` |
| 3. PJ-side state | kata (auto-written) | each PJ's `.kata/applied.toml` | Records which templates+revs were applied + var values used. **The truth** of what's installed |

### Truth lives in `.kata/applied.toml`

Borrowing yui's "target as truth" philosophy: the global config is
just a registry of PJ paths, never the truth of what's installed.
Each PJ's `applied.toml` is self-contained — clone the PJ, run
`kata apply`, get the same result without touching global config.
This is what makes kata teamwork-safe.

## Key design decisions (don't rediscover)

These were settled in the design conversation before this repo
existed. Flag with the user before reverting any of them.

- **`how` and `when` are orthogonal**, not a single mode enum.
  - `how` = `overwrite` | `merge-section` | `merge-toml` |
    `merge-yaml` | `ai` | `script` (the *method*)
  - `when` = `once` | `always` | `manual` (the *timing*)
  - This lets `how="ai", when="once"` (AI generates ROADMAP.md
    seed) and `how="script", when="once"` coexist naturally.
    Collapsing to one enum was rejected for forcing artificial
    coupling.

- **Templates compose, last wins.** A PJ lists templates in order;
  later layers can override files from earlier ones. `pj-base`
  carries language-agnostic basics, `pj-rust` overlays Rust
  specifics, `pj-rust-cli` adds CLI-only scaffolding. Same file
  with `merge-section` can carry per-layer managed blocks (each
  layer owns its block).

- **Preset = a bundle of template refs**, not a template. Solves
  the "I'd rather not retype 3 template URLs in every new PJ"
  problem without making templates themselves recursive. A preset
  is just a list — vars defaults are allowed but discouraged.

- **`teravars` is mandatory and authoritative.** `kata` does not
  re-implement Tera engine, `[vars]` extraction, or `include`
  resolution. Anything missing in `teravars` should be added
  *there*, not duplicated in `kata`. This keeps shun / rvpm /
  todoke / yui / renri / kata aligned on a single Tera convention.

- **AI mode delegates to the user's installed agent CLI**
  (`claude` / `gemini` / `codex`), via a swappable `AiAgent`
  trait. Each backend shells out (`tokio::process::Command`).
  `agent = "auto"` falls back claude > codex > gemini based on
  what's on `PATH`. Cache is **not** in MVP; add only when needed.

- **Interactive AI decision is chezmoi-style: `[a]ccept /
  [e]dit / [s]kip / [d]efer`.** `defer` records "ask again next
  time", not "permanently skip". `--yes` accepts everything;
  `--no-ai` skips all AI files entirely. `--non-interactive`
  defaults to skip (safe), `--non-interactive --yes` is the CI
  full-AI mode.

- **Parallelism is mandatory, tokio fan-out.** PJ-level via
  `JoinSet`; per-PJ file-level via `Semaphore`; AI calls share a
  global `Semaphore` (default 4) so we don't fork-bomb the
  agent CLI. Per-PJ stdout/stderr are buffered and flushed at PJ
  completion to avoid interleaving. Progress UI = `indicatif`
  `MultiProgress`, one row per PJ.

- **vars precedence (highest first):**
  `--var name=val` (CLI) > `KATA_VAR_<name>` (env) >
  `applied.toml` > `preset.vars` > `manifest.default` > prompt.
  CLI wins so `kata apply --var foo=bar` is always a valid
  one-shot override.

- **preset spec grammar:** `<source>[@<rev>][//<subdir>][:<preset-name>]`,
  Terraform-module style.
  Example: `github.com/yukimemi/pj-presets:rust-cli`,
  `github.com/x/y@v1.0//path:name`.
  Local paths (`./...`, `../...`) bypass git resolution.

- **Resilience principle (from rvpm):** a single failure must
  not stop the whole tool. Per-PJ failures are isolated; the
  end-of-run report aggregates them. **Exception**: half-applied
  state (some files written, then bail mid-PJ) leaves
  `applied.toml` inconsistent — for those, abort the PJ entirely
  and don't update `applied.toml`.

- **Don't reach for `git2` / `gix`.** Shell out to `git` (yui
  experience: libgit2 linking on Windows is more pain than it's
  worth). `git` CLI must exist; `kata doctor` checks.

- **`.kata/applied.toml` is committed** *— design intent.*
  Teammates cloning the PJ should be able to `kata apply` and
  reproduce the layout. The cache (`~/.cache/kata/`) is NOT,
  naturally.
  - **Phase 1 caveat**: until git-fetched templates land in Phase 2,
    `applied.toml` records *absolute local* paths for `preset` and
    `base_dir` (the templates literally only exist on the author's
    machine). For now kata's own dogfood gitignores `.kata/`; the
    moment Phase 2 makes templates portable, the field gets
    untracked-status reverted and the design returns to plan.
    See ROADMAP "Phase 1 follow-ups".

## Source layout (planned)

See [ROADMAP.md](./ROADMAP.md) for the full module breakdown. Top
level:

```
src/
  main.rs                 — entry, parses Cli, drives tokio runtime
  lib.rs                  — mod list + Error/Result re-export
  cli.rs                  — clap definitions
  cmd/                    — one file per subcommand (init/apply/status/...)
                            (split deliberately — yui's monolithic cmd.rs
                             grew to 6000+ lines, kata avoids that trap)
  config/                 — GlobalConfig + [[project]] registry
  preset.rs               — Preset (bundle of TemplateRef)
  manifest.rs             — template.toml schema (Manifest, FileSpec, VarSpec)
  applied.rs              — .kata/applied.toml (the truth)
  template/               — TemplateSource + TemplateCache
  render/                 — teravars wrapper + kata.* / project.* / system.* context
  modes/                  — ApplyMode trait + one impl per `how`
  ai/                     — AiAgent trait + claude / gemini / codex backends
  runner/                 — tokio fan-out (PJ × file × AI semaphores) + progress
  ui/                     — diff / icons / interactive prompt
  git.rs                  — shell-out git wrappers
  paths.rs                — global config / cache / pj root resolution
```

## Development

**Practice TDD.** Red-green-refactor.

```bash
cargo make setup                    # one-time on clone: hook + apm install
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo make check                    # all of the above (pre-push gate)
```

`cargo make setup` is `hook-install` + `apm-install`. The latter
requires the [APM](https://github.com/microsoft/apm) CLI on `PATH`
(`scoop install apm` on Windows, `brew install microsoft/apm/apm`
on macOS, etc.). It compiles renri's skill into `.claude/skills/` +
`.gemini/skills/` + `.github/skills/` so AI sessions know how to
manage worktrees while developing kata.

`kata` will eventually **dogfood itself** — `pj-presets:rust-cli`
applied to `yukimemi/{shun, rvpm, todoke, yui, renri, spyrun, kata}`.
Phase 4 milestone.

## Working in this repo with AI agents

Standard yukimemi flow:

- **Read-only inspection**: no worktree needed.
- **Any commit-bound change**: start with `renri add <branch-name>`
  and work in the worktree.
- Trivial typo fixes can edit the main checkout directly.

Backend choice — colocated git+jj, **jj-first** (renri default).

```sh
jj st
jj describe -m "feat: ..."
jj git push --bookmark <branch> --allow-new
```

## Resilience principle

Single failure should not stop the whole tool *unless* it would
leave inconsistent state.

- One PJ's apply fails → log, continue with siblings, surface in
  end-of-run report
- One file's `how` execution fails inside a PJ → abort that PJ
  entirely and roll back `applied.toml` (don't half-update)
- Template git fetch fails → skip that template, surface clearly
- AI backend unavailable → fall through to next agent in `auto`
  order, or skip the file with a clear message
- `kata status` must work even when `kata apply` would fail

## Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
  - Exception: standalone version bumps.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- Tag-based releases: `git tag vX.Y.Z && git push origin vX.Y.Z`.

### PR review cycle

Same as renri / yui — Gemini Code Assist + CodeRabbit auto-review,
reply with `@gemini-code-assist` / `@coderabbitai` after each fix
push, settle when threads quiet, merge when @yukimemi approves.

## Status

Pre-implementation. Design is settled (this file + ROADMAP.md).
Phase 1 (single-PJ overwrite + once/always) is the next coding
milestone.

[teravars]: https://github.com/yukimemi/teravars
