<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kata/main/assets/logo-dark.svg">
    <img src="https://raw.githubusercontent.com/yukimemi/kata/main/assets/logo.svg" alt="kata — multi-project template applier with AI-delegated merge" width="560">
  </picture>
</p>

> 型 — *the woodblock pattern*. A multi-project template
> applier with AI-delegated merge for the files that resist
> mechanical sync.

**Status:** pre-implementation. Design is settled in
[CLAUDE.md](./CLAUDE.md), implementation plan in
[ROADMAP.md](./ROADMAP.md).

## Why

Maintaining N sibling Rust / Go / Bun projects means every
boilerplate change to `Makefile.toml` / `apm.yml` / `renri.toml`
/ `CLAUDE.md` has to be copy-pasted across all of them. Existing
tools (copier, cookiecutter, cruft) cover the mechanical case but
nothing handles files that need *judgement* on update — like
`CLAUDE.md`, where each project has shared sections plus
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

## License

MIT.
