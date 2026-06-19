# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`AGENTS.md` is the canonical agent guide and overlaps heavily with this file —
read it too. `docs/plans/` holds the full design; the code is cross-referenced
to it by ID (see "Conventions").

## What this is

**fly** is a Linux desktop "terminal for AI coding agents": real PTY-backed
panes, tabs + splits (one agent per pane), and an attention indicator + OS
notification when an agent needs you. v1 wires **Claude Code** as the attention
source. Stack: **Tauri v2** (Rust backend) + **Svelte 5** (Vite/TS frontend) +
**xterm.js** terminal panes.

## Commands

```bash
pnpm install                  # frontend deps
pnpm tauri dev                # run the app (Vite dev server + cargo run) — use this, not a bare cargo build
pnpm check                    # svelte-check: type-check the frontend
pnpm test:unit                # vitest: all frontend unit tests
pnpm tauri build              # release .deb (primary) + AppImage (secondary)

cargo test --offline --manifest-path src-tauri/Cargo.toml          # all Rust tests
cargo test --offline --manifest-path src-tauri/Cargo.toml --test hook_auth   # one integration-test file (src-tauri/tests/<name>.rs)
cargo test --offline --manifest-path src-tauri/Cargo.toml <substr> # tests whose name matches <substr>

pnpm vitest run src/lib/keymap.test.ts          # one frontend test file
pnpm vitest run -t "leader"                      # frontend tests matching a name
```

### Environment gotchas (important — these will bite)

- **cargo + the Bash sandbox**: `index.crates.io` is blocked, so any build that
  resolves a *new* crate hangs. Run new-dependency builds with
  `dangerouslyDisableSandbox: true`; once deps are cached, `cargo
  build/test --offline` works sandboxed.
- **Run via `pnpm tauri dev`, never a bare debug `cargo build` binary** — the
  debug binary loads the frontend from the Vite `devUrl`, so standalone it shows
  a blank window. The release build embeds the frontend.
- **Known release-mode blank window on WebKitGTK 2.52** (this 24.04 dev box):
  `pnpm tauri build` produces a working `.deb`, but the release webview here
  fails to load embedded assets via Tauri's custom protocol. It's a WebKitGTK
  2.52 regression, not app code; the target baseline is Ubuntu 22.04. Dev mode
  works fully. (`vite.config.ts` strips `crossorigin` from the built HTML — a
  related, kept fix.)
- **The webview console is invisible.** The frontend forwards uncaught errors to
  app stderr via the `frontend_log` command — grep stderr for `[fly-webview]`.

## Architecture

### One binary, two roles
`main.rs` → `lib.rs::run()`. If argv[1] is a CLI subcommand (`notify`, `hooks`),
the process runs as the **`fly` CLI** and exits; otherwise it launches the
Tauri desktop app. The CLI and the app share the same `fly_lib` crate, so a `fly
notify` invocation inside a pane talks to the running app.

### The attention pipeline (the core feature, spans many files)
This is the data flow that makes an agent's "I need you" reach the UI:

1. `fly hooks setup` installs a Claude Code `command` hook in
   `~/.claude/settings.json` (backed up first) that runs `fly notify`.
2. When a pane spawns (`stream::spawn_pane`), the backend mints a per-pane
   CSPRNG token and injects `FLY_PANE_TOKEN` + `FLY_SOCKET_PATH` into the child
   env, and registers the pane with the `AttentionManager`.
3. Claude Code's `Notification`/`Stop` event fires `fly notify`, which connects
   to the **authenticated Unix socket** (`hooks/`): constant-time token compare
   + `SO_PEERCRED` + lockout. This is the security boundary — treat it as such.
4. `HookServer`'s dispatch feeds a `Signal` into `AttentionManager::signal`,
   which runs the pure attention state machine and the **suppression matrix**
   (`state/suppress.rs`) against the focus/foreground tuple replicated from the
   frontend.
5. On a raise it emits `pane://attention` to the frontend (ring on the pane/tab)
   and, if not suppressed, surfaces an OS notification through the
   `NotificationGate` (coalesce when many panes are raised; rate-limit bursts).

Attention has a **tiered confidence model** (`Tier`: Hook/Cli/Bel/Osc — only
`Hook` is produced in v1; the rest are forward design). Each pane is modeled by
**two orthogonal pure state machines**: `state/lifecycle.rs` (process status)
and `state/attention.rs` (Idle→Raised→Acknowledged). Both take time/inputs as
arguments so they're tested without a running app.

### Backend modules (`src-tauri/src/`)
- `pty/` — `PtyManager` registry + `Pane`: portable-pty, one read thread per
  pane, backpressure pause/resume (watermarks), ordered reap-on-exit.
- `stream/` — `spawn_pane`, raw-byte PTY output over a Tauri `Channel` (no
  transcoding), and the pane↔attention/focus wiring + Tauri commands.
- `state/` — the two state machines + suppression matrix + per-pane `manager`.
- `hooks/` — the authenticated socket: `token`, `protocol`, `server`.
- `cli/` — `fly notify`, `fly hooks setup|teardown`.
- `notify/`, `config/`, `session/`, `cwd/` (via `/proc`), `lifecycle.rs`
  (ordered shutdown — reap every pane, no zombies/orphans).
- All Tauri commands are registered in the `invoke_handler!` in `lib.rs`; the
  frontend's typed wrappers for them live in `src/ipc.ts`. Add a command in both
  places.

### Frontend (`src/`)
- `App.svelte` — orchestrates tabs and the split tree; owns attention/cwd state,
  debounced session persistence (~800ms), and overlay (hotkey menu / close-tab
  confirm) wiring.
- `lib/layout.ts` — **pure split-tree model**. Leaves render flat and keyed, so
  splitting/resizing never unmounts a pane (which would respawn its agent). Leaf
  keys are stable and also key the scrollback files — preserve this invariant.
- `lib/keymap.ts` — the leader-key model (tmux-style: default Ctrl-A, then a
  command key; everything else passes through to the PTY). `BINDINGS` is the
  single source of truth shared by `dispatch()` and the hotkey menu, so they
  cannot drift.
- `lib/Terminal.svelte` — embeddable xterm leaf; subscribes to `pane://attention`.
- `lib/{config,serialize}.ts`, `lib/{TabBar,HotkeyMenu}.svelte`.

## Conventions

- Code is cross-referenced to the design by ID — **KTD\<n\>** (key technical
  decision) and **R\<n\>**/**U\<n\>** (requirement/unit) appear in doc comments
  and tie back to `docs/plans/`. When changing behavior, keep the referenced IDs
  accurate. Match the surrounding style; modules are heavily doc-commented.
- Behavior-bearing units ship with tests (Rust state machines are test-first and
  pure; frontend has vitest for layout/keymap).
- Commits: conventional, with a `Co-Authored-By: Claude` trailer.
