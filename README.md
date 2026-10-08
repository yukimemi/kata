<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kata/main/assets/logo-dark.svg">
    <img src="https://raw.githubusercontent.com/yukimemi/kata/main/assets/logo.svg" alt="kata — multi-project template applier with AI-delegated merge" width="560">
  </picture>
</p>

<p align="center">
  <a href="https://deepwiki.com/yukimemi/kata"><img src="https://deepwiki.com/badge.svg" alt="Ask DeepWiki"/></a>
  <a href="https://codewiki.google/github.com/yukimemi/kata"><img src="https://img.shields.io/badge/View-Code_Wiki-4285F4?logo=google" alt="View Code Wiki"/></a>
</p>

> 型 — *the woodblock pattern*. A multi-project template applier
> with AI-delegated merge for the files that resist mechanical
> sync.

**Status: Phase 1 MVP shipped (local presets, `overwrite` mode,
`once` / `always` timing). Not yet on crates.io. See
[ROADMAP.md](./ROADMAP.md) for what's next (git-fetched templates,
`merge-section` / `merge-toml` / `merge-yaml` / `script` modes,
AI delegation, multi-PJ parallelism). Design notes in
[AGENTS.md](./AGENTS.md).**

## Why

Maintaining N sibling Rust / Go / Bun projects means every
boilerplate change to `Makefile.toml` / `apm.yml` / `renri.toml`
/ CI / `AGENTS.md` has to be copy-pasted across all of them.
Existing tools (copier, cookiecutter, cruft) cover the mechanical
case but nothing handles files that need *judgement* on update —
like `AGENTS.md`, where each project has shared sections plus
project-specific ones.

`kata` aims to:

- **Layer templates** — `pj-base` + `pj-rust` + `pj-rust-cli`
  compose, last layer wins. Avoid duplicating the common bits per
  language.
- **Two-axis modes** — `how` (`overwrite` / `merge-section` /
  `merge-toml` / `merge-yaml` / `ai` / `script`) and `when`
  (`once` / `always` / `manual`) are independent. `how="ai",
  when="once"` and `how="script", when="always"` both make sense.
- **Delegate the un-mergeable** — files marked `how = "ai"` get
  handed to your installed `claude` / `gemini` / `codex` CLI with
  the template diff and current contents. You see a diff and pick
  `[a]ccept / [e]dit / [s]kip / [d]efer` (chezmoi-style).
- **Run in parallel** — tokio fan-out across projects, with
  semaphores limiting AI concurrency so the agent CLI doesn't get
  fork-bombed.
- **Truth lives with the project** — each project's
  `.kata/applied.toml` records what was applied. The global
  registry is just paths.

## Quick start

```sh
# Build from source (not on crates.io yet)
git clone https://github.com/yukimemi/kata
cd kata
cargo install --path .

# Apply a preset to a fresh project. Both local paths and git
# URLs work — kata clones the preset (and its referenced
# templates) into ~/.cache/kata/templates/ on first use.
mkdir my-rust-cli && cd my-rust-cli

# git URL form (clones pj-presets + every template it references):
kata init github.com/yukimemi/pj-presets:rust-cli --non-interactive

# or local path form (handy when iterating on a preset locally):
# kata init ~/src/github.com/yukimemi/pj-presets/rust-cli.toml

# Re-apply later when the templates evolve (idempotent; uses the
# cached clone — `kata update` to refresh)
kata apply --non-interactive

# Preview without writing
kata status

# Inspect what's tracked
kata list
```

The `<source>:<preset-name>` syntax is the same Terraform-module-style
spec [described in AGENTS.md](./AGENTS.md): `<source>[@<rev>][//<subdir>][:<preset-name>]`.

## Ignoring a template-managed file

A repo that deliberately does not want a template file (say, the CI
workflows on a private repo with no Actions minutes) can tell kata so:

```sh
kata ignore .github/workflows/ci.yml      # asks, deletes the file, records the marker
kata ignore -y .github/workflows/release.yml .github/workflows/auto-tag.yml
kata unignore .github/workflows/ci.yml    # clear the marker
```

`ignore` records `ignored = true` for the file in `.kata/applied.toml`:

```toml
[files.".github/workflows/ci.yml"]
ignored = true
```

- `kata apply` skips an ignored file entirely: it is not written and not
  recreated, whatever its `how` / `when`. `--reseed` does not override
  it; only `kata unignore` does. When several template layers target the
  same destination, all of them stop.
- If an ignored file still exists on disk, `apply` and `status` say
  `ignored (present on disk, untouched)` and leave it alone.
- `kata status` shows ignored files as `ignored` (never as drift or
  missing), `kata status --all` lists them under each project, and
  `kata list` marks them `(ignored)`, so a deliberate omission is not
  forgotten.
- `kata unignore` only clears the marker. The next `kata apply` follows
  the file's normal `when` rules: an `always` file comes back; a `once`
  file that was already applied still needs `--reseed`.
- Only files kata already knows (in the current templates or recorded in
  `applied.toml`) can be ignored. In `--non-interactive` mode `ignore`
  needs `--yes` when it has to delete a file.
- Older kata versions do not know the field and may restore the file.

## Global config

`~/.config/kata/config.toml` holds tool defaults under `[defaults]`
(plus the `[[project]]` registry). Auto-update knobs:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `auto_update` | `"off"` / `"notify"` / `"install"` | `"install"` | Background update behaviour at the start of each command. `install` (default) silently downloads + swaps the binary; the running process keeps the old binary and the new version applies on the next launch (one stderr line is printed only when an install happened). `notify` prints a banner pointing at `kata self-update` and never installs. `off` does nothing. Network / lock failures are swallowed silently; dev builds are never updated |
| `update_check_interval` | duration string (`"24h"`, `"6h"`, `"1d"`) | `"24h"` | Minimum interval between background checks (humantime format). The last-check timestamp is persisted under kata's cache dir. Invalid values fall back to `"24h"` |

```toml
[defaults]
auto_update = "install"      # off | notify | install
# update_check_interval = "24h"
```

Set `KATA_NO_AUTOUPDATE=1` in the environment to disable the
background check for a single invocation — it overrides `auto_update`
to `off` regardless of config. (Non-empty and not `0` / `false`.)

Run `kata self-update [--yes] [--check]` to update on demand: `--yes`
skips the confirmation prompt, `--check` reports availability without
installing.

## Companion repos

| Repo | Role |
|---|---|
| [`yukimemi/pj-base`](https://github.com/yukimemi/pj-base) | Language-agnostic boilerplate (LICENSE, …) |
| [`yukimemi/pj-rust`](https://github.com/yukimemi/pj-rust) | Rust language layer (Makefile.toml, CI matrix, rust-toolchain) |
| [`yukimemi/pj-rust-cli`](https://github.com/yukimemi/pj-rust-cli) | Rust CLI extras (.editorconfig, …) |
| [`yukimemi/pj-rust-lib`](https://github.com/yukimemi/pj-rust-lib) | Rust library extras (crates.io publish, no binaries) |
| [`yukimemi/pj-pnpm`](https://github.com/yukimemi/pj-pnpm) | pnpm / TypeScript language layer (package.json, tsconfig refs, pnpm renri hook) |
| [`yukimemi/pj-react-web`](https://github.com/yukimemi/pj-react-web) | Vite + React + TS + Tailwind framework layer |
| [`yukimemi/pj-firebase`](https://github.com/yukimemi/pj-firebase) | Firebase Hosting + Firestore + Storage + Vercel mirror |
| [`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets) | Preset bundles (`rust-cli`, `rust-lib`, `web-react`, `web-react-firebase`) |

`kata` itself dogfoods these — the `Makefile.toml` / CI / etc. in
this repo are managed by `kata apply` from `pj-presets:rust-cli`.

## License

MIT.
