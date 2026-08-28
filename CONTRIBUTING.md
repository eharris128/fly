# Contributing to fly

Thanks for your interest. fly is a Linux-only desktop terminal for AI coding
agents: a Rust core, an Electron shell, and a Svelte/xterm.js frontend. This
file is the short on-ramp; the full guide is [`CLAUDE.md`](CLAUDE.md).

## Read `CLAUDE.md` first

`CLAUDE.md` is written for coding agents, but it is the primary contributor
guide for humans too: every command, the architecture (the attention pipeline,
the hook-socket security boundary, the control socket, automations, the tmux
substrate), the environment gotchas, and the conventions below in full.
`AGENTS.md` is a pointer to it for non-Claude tools. Read it before opening
anything larger than a typo fix.

## Dev setup

```bash
pnpm install                                     # frontend + Electron shell (one pnpm workspace)
cargo build --manifest-path core/Cargo.toml      # the debug core the dev shell spawns
pnpm dev                                         # Vite on :1420            (terminal 1)
pnpm shell:dev                                   # Electron, dev flavor     (terminal 2)
```

The dev shell runs as the `fly-el` flavor, fully isolated from an installed
release (config, session, sockets) — see `CLAUDE.md` → "Stable + dev side by
side". `pnpm build:deb` produces the installable package. System deps and the
things that will bite you (cargo behind a sandbox, why `cargo run` never opens
a window, where the logs go) are in `CLAUDE.md` → "Commands".

## Tests

```bash
pnpm check                                                   # svelte-check (types)
pnpm test:unit                                               # vitest: frontend + electron/*.test.js + version lockstep
cargo test --offline --manifest-path core/Cargo.toml         # Rust: state machines, socket auth, feed, automations, …
```

Behavior-bearing units ship with tests. The Rust state machines are pure and
time-injected so they test without a running app; frontend view-models
(`src/lib/*.ts`) are framework-free for the same reason. A change to
`core/src/hooks/` or `core/src/feed/` is a change to a trust boundary — read
`core/src/hooks/CLAUDE.md` and keep the existing tests' shape.

## Conventions

- **Design IDs.** Code is cross-referenced to `docs/plans/` by `KTD<n>` /
  `R<n>` / `U<n>` in doc comments. IDs are scoped per plan
  (`docs/plans/README.md` maps plans to code). When you change behavior, keep
  the referenced IDs accurate; when you add a design decision, write it down.
- **Commits.** Conventional commits (`feat(scope): …`, `fix: …`, `docs: …`).
- **Versions.** `package.json`, `electron/package.json`, and `core/Cargo.toml`
  stay on the same version — `src/version-lockstep.test.ts` fails otherwise.
- **README.** When a change moves the product story (shell, install path, CLI
  surface, headline features), update `README.md` too.

## Security issues

Do not open a public issue for a vulnerability — see [`SECURITY.md`](SECURITY.md).

## License

By contributing you agree your contributions are licensed under the
[MIT License](LICENSE).
