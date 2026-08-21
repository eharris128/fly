# Building fly on macOS

A self-serve path to build and try fly on a Mac. macOS is a **best-effort,
partially-ported target** — the hook socket security boundary is fully ported
(`hooks/server.rs` uses `getpeereid(2)` in place of Linux's `SO_PEERCRED`),
but the `/proc`-based subsystems degrade (see "Known limitations" below).
Linux remains the primary target.

> **Post-cutover status (2026-08-21):** this doc builds the **Tauri** shell,
> which on Linux is now the rollback path — the shipped Linux product is the
> Electron shell + `fly core` (`electron/`). No Electron packaging exists for
> macOS yet; on a Mac the Tauri path below is still the (only) way to run
> fly, and it remains fully supported for that purpose. The migration plan
> (`docs/plans/2026-08-12-002-…`) expects a future macOS Electron path to be
> a straight port; nothing here has been adapted for it.

## Prerequisites (one-time)

```bash
xcode-select --install                                  # Apple clang + SDK
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
brew install pnpm                                       # or: corepack enable
```

No Xcode.app, Apple Developer account, or signing identity is needed — a
locally built app is not quarantined, so Gatekeeper does not block it.

## Build & run

```bash
git clone git@github.com:eharris128/fly.git && cd fly
pnpm install
pnpm tauri dev        # iterate: Vite dev server + cargo run
pnpm build:mac        # bundle: .app + .dmg (fast release-dev profile)
```

`pnpm build:mac` produces
`src-tauri/target/release-dev/bundle/macos/fly.app` and a `.dmg` next to it in
`bundle/dmg/`. Drag the `.app` to `/Applications` or run it in place. For a
full fat-LTO build use `pnpm tauri build --bundles app dmg` instead.

To wire up the attention pipeline, run `fly hooks setup` from the built
binary once (e.g. `src-tauri/target/release-dev/fly hooks setup` or
`/Applications/fly.app/Contents/MacOS/fly hooks setup`). Hook setup writes the
binary's **absolute path** into `~/.claude/settings.json`, so `fly` does not
need to be on `PATH` — but re-run setup if you move the binary.

## Known limitations on macOS (phase 1)

Ported and expected to work: panes/tabs/splits, the attention pipeline
(hooks → authenticated socket → ring/notification), session persistence,
resume/handoff stores, the feed server, automations *scheduling* (the sweep,
the store, and script-mode runs — but see the agent-mode limitation below).

Degraded — these read Linux's `/proc`, which doesn't exist on macOS, and
currently fail soft (feature inert, no crash):

- **Pane cwd tracking** (`cwd/`): breadcrumb cwd and cwd-keyed transcript
  attribution won't resolve.
- **Dashboard agent detection & `running · N tasks`** (`cwd/` process table):
  panes won't classify as agents, so dashboard rows and the feed roster stay
  empty.
- **Headless agent runs** (`automations/headless.rs`): child liveness pinning
  (pid + `/proc/<pid>/stat` start-time) and the descendant kill sweep read
  `/proc`. One runner serves both monitor checks and ordinary agent-mode
  automations, and headless is the **default disposition** for every agent
  automation since the 2026-07-31 plan — so *all* agent-mode automations, not
  just monitors, are unusable on macOS yet. Script-mode automations are
  unaffected.

Full parity needs a process-inspection seam with a `libproc` backend — see
the macOS discussion in the repo history before starting that work.

## Caveat

`cargo check --target aarch64-apple-darwin` from Linux cannot fully verify
this target: the `objc2-exception-helper` dependency compiles Objective-C in
its build script, which needs a Darwin toolchain. The first `cargo` build on
an actual Mac is the real compile gate; if it fails, the fix likely belongs in
one of the files listed by `grep -rl 'libc::' src-tauri/src`.
