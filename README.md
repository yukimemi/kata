<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kata/main/assets/logo-dark.svg">
    <img src="https://raw.githubusercontent.com/yukimemi/kata/main/assets/logo.svg" alt="kata — multi-project template applier with AI-delegated merge" width="560">
  </picture>
</p>

> 型 — *the woodblock pattern*. A multi-project template applier
> with AI-delegated merge for the files that resist mechanical
> sync.

**Status: Phase 1 MVP shipped (local presets, `overwrite` mode,
`once` / `always` timing). Not yet on crates.io. See
[ROADMAP.md](./ROADMAP.md) for what's next (git-fetched templates,
`merge-section` / `merge-toml` / `merge-yaml` / `script` modes,
AI delegation, multi-PJ parallelism). Design notes in
[CLAUDE.md](./CLAUDE.md).**

## Why

Maintaining N sibling Rust / Go / Bun projects means every
boilerplate change to `Makefile.toml` / `apm.yml` / `renri.toml`
/ CI / `CLAUDE.md` has to be copy-pasted across all of them.
Existing tools (copier, cookiecutter, cruft) cover the mechanical
case but nothing handles files that need *judgement* on update —
like `CLAUDE.md`, where each project has shared sections plus
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

## Phase 1 quick start

Phase 1 supports **local presets only** — point `kata init` at a
preset file on disk:

```sh
# Build from source (not on crates.io yet)
git clone https://github.com/yukimemi/kata
cd kata
cargo install --path .

# Apply a preset to a fresh project
mkdir my-rust-cli && cd my-rust-cli
kata init ~/src/github.com/yukimemi/pj-presets/rust-cli.toml \
  --non-interactive

# Re-apply later when the templates evolve (idempotent)
kata apply --non-interactive

# Preview without writing
kata status

# Inspect what's tracked
kata list
```

## Companion repos

| Repo | Role |
|---|---|
| [`yukimemi/pj-base`](https://github.com/yukimemi/pj-base) | Language-agnostic boilerplate (LICENSE, …) |
| [`yukimemi/pj-rust`](https://github.com/yukimemi/pj-rust) | Rust language layer (Makefile.toml, CI matrix, rust-toolchain) |
| [`yukimemi/pj-rust-cli`](https://github.com/yukimemi/pj-rust-cli) | Rust CLI extras (.editorconfig, …) |
| [`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets) | Preset bundles (`rust-cli.toml`, …) |

`kata` itself dogfoods these — the `Makefile.toml` / CI / etc. in
this repo are managed by `kata apply` from `pj-presets:rust-cli`.

## License

MIT.
