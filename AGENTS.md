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

## Status

Phase 3 shipped (AI delegation MVP). v0.2.0 on crates.io.

The git-workflow / PR-review-cycle / worktree / agents.md
conventions used to live here as hand-written sections; they
moved to the kata-managed `<!-- kata:agents:base:* -->` block
below so every yukimemi/* repo sees the same guidance. Edit them
in `yukimemi/pj-base/AGENTS.md.base`, not in this file.

[teravars]: https://github.com/yukimemi/teravars
<!-- kata:agents:base:begin -->
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
- **Releases are PR-driven, tagging is automatic.** Bump
  `[workspace.package].version` (workspace) or `[package].version`
  (single crate) in a `chore/release-vX.Y.Z` PR. On merge to `main`,
  `.github/workflows/auto-tag.yml` (kata-managed) detects the bump,
  pushes the `vX.Y.Z` tag, and that tag fires `release.yml` for
  binary builds + crates.io publish. **Do not run `git tag` by
  hand** — the bot tag will collide and the manual push fails.

### PR review cycle

- Every PR runs reviews from **Gemini Code Assist** and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved.
- **After opening a PR, immediately enter the review-monitoring
  loop — do not ask the user whether to start it.** Drive the
  cadence with `/loop` — fixed-interval mode (e.g.
  `/loop 60s …`) schedules ticks via `CronCreate`; dynamic mode
  (no interval, `/loop …`) self-paces via `ScheduleWakeup`. The
  agent actively pulls fresh state each tick with
  `gh pr view <N> --json state,reviews,comments,statusCheckRollup`
  and `gh api repos/<owner>/<repo>/pulls/<N>/comments` (the
  latter covers inline review comments, which `gh pr view`
  does not surface) and reacts to new bot feedback. Passive
  watchers (background `gh` polls, file watchers, hooks) cannot
  trigger active follow-up, so they are not a substitute —
  without an active wake-up the agent never re-reads the PR.
- **Default polling interval: 60s.** Gemini Code Assist /
  CodeRabbit historically reply within ~1–3 minutes of a push or
  thread reply, so a 60s tick catches them on the next wake-up
  without burning cache: 60s sits well inside the 5-minute
  prompt-cache TTL, so the conversation context stays cached
  across ticks. Do **not** stretch the interval to 300s — that
  is the worst-of-both window (you pay the cache miss without
  amortizing it). If the PR is idle but a bot re-review is still
  expected (e.g. a CodeRabbit rate-limit refill window), step
  **up** to 1200–1800s instead.
- **Stop the loop entirely when only owner approval is missing.**
  Once review bots are quiet (or quiet-by-exception — version-bump
  skip, Renovate/Dependabot skip), CI is green, and there is no
  other expected follow-up, the *only* remaining action is human
  approval. GitHub already notifies the owner; the agent
  re-entering on every cron tick to find the same "still waiting
  on owner" state burns cache and adds no value. Stop scheduling
  further wake-ups (`CronDelete` in fixed-interval mode; simply
  omit the next `ScheduleWakeup` in dynamic mode) and report the
  wait state to the user. The owner restarts the loop after their
  next push if a fresh bot pass is wanted, or merges directly.
  (A CodeRabbit rate-limit window doesn't qualify on its own — a
  re-review is still expected once the quota refills, so step up
  to 1200–1800s instead and let it ride. Stopping is only correct
  when the owner has explicitly chosen to skip the bot pass per
  the rate-limit exception below.)
- **Reply to reviewers after pushing a fix.** Reply on the
  corresponding review thread with an **@-mention**
  (`@gemini-code-assist` / `@coderabbitai`). Silent fixes are
  invisible to reviewers and cost the audit trail.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.
- **Version-bump-only PRs** (a single `chore/release-vX.Y.Z`
  branch whose entire diff is `[workspace.package].version` /
  `[package].version` + the matching inter-crate refs +
  `Cargo.lock`) **also skip the bot-review gate.** There is
  nothing for the bots to find in a version bump, and the
  release pipeline downstream of merge (auto-tag → release.yml)
  is time-sensitive. CI green + owner approval is enough.
- **Treat CodeRabbit rate-limit notices as "quiet" for the
  merge gate.** If CodeRabbit only posts a "Review limit
  reached" quota-exhaustion message (no findings, no inline
  comments), it has produced no review content — there is
  nothing to address. Re-trigger with `@coderabbitai review`
  once the quota refills if you want a real pass; for small or
  time-sensitive PRs, merge on owner approval without waiting.

### Worktree workflow

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name>            # create a worktree (jj-first)
renri --vcs git add <branch-name>  # force a git worktree
renri remove <branch-name>         # cleanup after merge
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

### kata-managed sections

Several files in this repo are managed by `kata apply` from the
[`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets)
templates — the bytes between `<!-- kata:*:begin -->` and
`<!-- kata:*:end -->` markers, plus the overwrite-always files
listed in `.kata/applied.toml`. **Editing those bytes locally
won't survive the next `kata apply`** — push the change to the
upstream template repo (`yukimemi/pj-base` / `yukimemi/pj-rust` /
…) instead. The marker scopes are layered:

- `kata:agents:base:*` — language-agnostic conventions (this section).
- `kata:agents:rust:*` — added when `pj-rust` applies.
- `kata:agents:rust-cli:*` — added when `pj-rust-cli` applies.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the shared Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # fmt --check + clippy + test + lock-check (the pre-push gate)
cargo make setup                    # one-time hook install + apm install
cargo build                         # debug build
cargo build --release               # release build
cargo test                          # tests; add -- --nocapture for stdout
```

`cargo make check` is what `.github/workflows/ci.yml` runs and what
the local pre-push hook calls — anything that passes locally
should pass on CI and vice versa. Don't paper over a failing
clippy by sprinkling `#[allow(clippy::...)]`; fix the underlying
issue or push back on the lint with reasoning.

### Toolchain pin

The Rust toolchain is pinned via `rust-toolchain.toml` and the
project compiles with the `stable` channel. Don't introduce
nightly-only features without a real reason; if you do, document
the reason in the relevant module.

### Lint / format policy

`rustfmt.toml` and `clippy.toml` are kata-managed (sourced from
`yukimemi/pj-rust`). Edits to those files in this repo won't
survive the next `kata apply`; if a setting is wrong, push the
fix to `yukimemi/pj-rust` so every Rust project using these templates picks
it up.

### CI workflow

`.github/workflows/ci.yml` is also kata-managed. The source lives
in `yukimemi/pj-rust/.github/workflows/ci.yml.template` (the
`.template` suffix keeps GitHub Actions from running the source
itself in pj-rust); each Rust project receives the rendered
`ci.yml` via `kata apply`. Action versions are bumped centrally
by Renovate at `yukimemi/pj-rust` and propagate down on the next
apply, so don't bump them locally — Renovate is configured
(via the kata-distributed `renovate.json`) to ignore
`.github/workflows/ci.yml` and `.github/workflows/release.yml`
in each PJ to avoid the bump→clobber loop.

### Releasing: version bump PR + auto-tag

Releases are triggered from `main` by a Cargo.toml version
change. `.github/workflows/auto-tag.yml` is kata-managed (source:
`yukimemi/pj-rust/.github/workflows/auto-tag.yml.tera`). It
watches `main` and, whenever a commit lands that changes the
top-level `version = "..."` in `Cargo.toml`, it pushes a matching
`vX.Y.Z` tag — no manual `git tag` step is needed. The tag push
then fires `release.yml`; see `kata:agents:rust-lib:*` or
`kata:agents:rust-cli:*` for what release.yml does in each
crate shape.

Cut a release via a small PR — never `git push` the bump
straight to `main`, even though the base block lists version
bumps as an exception to "no direct push". `auto-tag.yml` only
fires on `main`-branch pushes, so the bump must land via a merge
either way; using a PR also gives CI a chance to gate the
release. Enable automerge so CI green = release start:

```sh
git switch -c chore/bump-X.Y.Z
# Edit `package.version` in Cargo.toml, then:
cargo build                     # let Cargo.lock follow
git commit -am "chore: bump version to X.Y.Z"
git push -u origin chore/bump-X.Y.Z
gh pr create --fill
gh pr merge --auto --squash --delete-branch
```

Once CI is green the PR auto-merges. `auto-tag.yml` then pushes
`vX.Y.Z`, which fires `release.yml`.

**Repo settings to set once:** enable
`delete_branch_on_merge=true` (Settings → General →
"Automatically delete head branches"). The `--delete-branch`
flag on `gh pr merge --auto` is effectively a no-op — gh
returns as soon as automerge is enabled, so the deletion has to
happen server-side, which requires the repo setting.

**Why `KATA_APPLY_TOKEN`:** GitHub refuses to fire downstream
workflows from tags pushed by the default `GITHUB_TOKEN`, so
`auto-tag.yml` pushes with `KATA_APPLY_TOKEN` (the same PAT
`kata-apply.yml` already uses). Each consumer repo needs a
`KATA_APPLY_TOKEN` secret set; if a version-bump merge silently
doesn't fire `release.yml`, the missing PAT is the first thing
to check.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

Releases are triggered by a Cargo.toml version bump landing on
`main`. The bump flow itself (PR with automerge → `auto-tag.yml`
pushes `vX.Y.Z` → `release.yml` runs) is documented in
`kata:agents:rust:*` under "Releasing: version bump PR +
auto-tag" — that block also covers the `KATA_APPLY_TOKEN` and
`delete_branch_on_merge` setup. What `release.yml` then does for
a **CLI** crate:

1. Cross-compiles binaries for x86_64 Linux / Windows / macOS,
   plus aarch64 macOS (Apple Silicon) — full triples
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
   `x86_64-apple-darwin`, `aarch64-apple-darwin`.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first release. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across CLIs using these templates unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.

### Release smoke target (`examples/smoke.rs`)

After `cargo build --release`, `release.yml` runs
`cargo run --release --target <T> --example smoke` on every build
matrix entry. `cargo test` runs only library code, so the produced
binary's startup path goes unverified — that's how shoka v0.10.0
shipped a rustls `CryptoProvider` panic to crates.io even though
all 13 CI checks were green.

The template's default `examples/smoke.rs` body is intentionally
no-op so kata can drop it into every consumer crate without
breaking releases. **Override it per crate** with the smallest
operation that exercises the regression-prone surface:

- HTTPS-using CLIs: build the API client (octocrab, reqwest, etc.)
  and issue a tiny no-auth GET — that forces the rustls handshake
  to run inside the same binary the release publishes.
- File-handling CLIs: write+read a temp file via the real I/O
  helpers (catches missing crate features, permission regressions).
- Pure library crates: leave as no-op.

A failing smoke blocks the release before publishing to GitHub
Releases / crates.io.
<!-- kata:agents:rust-cli:end -->
