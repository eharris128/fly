# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`docs/plans/` holds the full design; the code is cross-referenced to it by ID
(see "Conventions"). This file is the single agent guide for the repo.

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
pnpm flavor:dev               # run a dev build ALONGSIDE an installed release (see "Stable + dev side by side")
pnpm check                    # svelte-check: type-check the frontend
pnpm test:unit                # vitest: all frontend unit tests
pnpm tauri build --bundles deb   # standalone .deb (skip AppImage — it needs network at bundle time)

cargo test --offline --manifest-path src-tauri/Cargo.toml          # all Rust tests
cargo test --offline --manifest-path src-tauri/Cargo.toml --test hook_auth   # one integration-test file (src-tauri/tests/<name>.rs)
cargo test --offline --manifest-path src-tauri/Cargo.toml <substr> # tests whose name matches <substr>

pnpm vitest run src/lib/keymap.test.ts          # one frontend test file
pnpm vitest run -t "leader"                      # frontend tests matching a name
```

**System deps** (Tauri/WebKitGTK on Ubuntu): `libwebkit2gtk-4.1-dev
build-essential libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
patchelf`.

### Environment gotchas (important — these will bite)

- **cargo + the Bash sandbox**: `index.crates.io` is blocked, so any build that
  resolves a *new* crate hangs. Run new-dependency builds with
  `dangerouslyDisableSandbox: true`; once deps are cached, `cargo
  build/test --offline` works sandboxed. (Caution: `dangerouslyDisableSandbox`
  + a foreground `sleep` in the same Bash call has been seen to abort with exit
  144 — keep sleeps out of sandbox-disabled commands.)
- **Run via `pnpm tauri dev`, never a bare debug `cargo build` binary** — the
  debug binary loads the frontend from the Vite `devUrl`, so standalone it shows
  a blank window. The release build embeds the frontend and runs standalone.
- **Release builds render fine here** (verified on this 24.04 box, Wayland +
  X11). An earlier blank-window issue on WebKitGTK 2.52 was the `crossorigin`
  module-script attribute failing to load over Tauri's custom asset protocol;
  the `fly-strip-crossorigin` Vite plugin in `vite.config.ts` strips it and
  fixes it. If a release build ever shows blank on Wayland, `GDK_BACKEND=x11
  fly` is a proven fallback.
- **Wayland screenshots are locked down** (GNOME 46: `org.gnome.Shell.Screenshot`
  → AccessDenied; x11grab of rootless Xwayland is black). To capture the app,
  launch it with `GDK_BACKEND=x11` (makes it an Xwayland client), then
  `xwd -id <winid> -out f.xwd && ffmpeg -i f.xwd f.png`.
- **The webview console is invisible.** The frontend forwards uncaught errors to
  app stderr via the `frontend_log` command — grep stderr for `[fly-webview]`.

## Packaging & running a stable build

`pnpm tauri build --bundles deb` produces
`src-tauri/target/release/bundle/deb/fly_<ver>_amd64.deb`. Install it
(`sudo apt install ./…deb`) for a standalone `/usr/bin/fly` + launcher,
independent of the source tree.

### Stable + dev side by side

A normal `pnpm tauri dev` shares the installed app's Tauri **identifier**
(`dev.evan.fly`) and its config/session/socket dirs, so the `single-instance`
plugin (registered in `lib.rs`) would just focus the installed window instead of
opening a second one, and the two would clobber each other's saved tabs. To run
an iterating dev build next to an installed stable app, use **`pnpm flavor:dev`**:

- `src-tauri/tauri.dev.conf.json` (merged via `tauri dev --config`) gives the
  dev build a distinct identifier `dev.evan.fly-dev` → its own single-instance
  lock, so both windows coexist.
- `FLY_APP_NAME=fly-dev` (set by the script) isolates its on-disk state. All
  three path roots derive from `lib.rs::app_dir_name()` (default `fly`,
  overridable via `FLY_APP_NAME`): config `~/.config/<app>/`, session/scrollback
  `~/.local/share/<app>/`, and the hook socket under `$XDG_RUNTIME_DIR/<app>/`.
  The installed release leaves `FLY_APP_NAME` unset → stays on `fly`.
- The dev window's title becomes `fly (dev)` (set at runtime in `lib.rs` setup
  when the flavor isn't `fly`) so it's distinguishable from the stable window.

The per-pane hook socket is also PID-keyed, so it never collides regardless.

## Architecture

### One binary, two roles
`main.rs` → `lib.rs::run()`. If argv[1] is a CLI subcommand (`notify`, `hooks`,
`automation`), the process runs as the **`fly` CLI** and exits; otherwise it
launches the Tauri desktop app. The CLI and the app share the same `fly_lib`
crate, so a `fly notify` invocation inside a pane talks to the running app.

`fly automation <create|list|show|runs|pause|resume|run|delete>` (U9) manages
cron-scheduled runs: read ops work anywhere (they read the store file directly),
mutating ops must run inside a pane (they post over the token-authenticated hook
socket). See the Automations module map below.

### The attention pipeline (the core feature, spans many files)
This is the data flow that makes an agent's "I need you" reach the UI:

1. `fly hooks setup` installs a Claude Code `command` hook in
   `~/.claude/settings.json` (backed up first) that runs `fly notify`.
   `fly hooks teardown` removes only fly's hooks.
2. When a pane spawns (`stream::spawn_pane`), the backend mints a per-pane
   CSPRNG token and injects `FLY_PANE_TOKEN` + `FLY_SOCKET_PATH` into the child
   env, and registers the pane with the `AttentionManager`.
3. Claude Code's `Notification`/`Stop` event fires `fly notify`, which connects
   to the **authenticated Unix socket** (`hooks/`): constant-time token compare
   + `SO_PEERCRED` + lockout. This is the security boundary — treat it as such.
4. `HookServer`'s dispatch feeds a `Signal` into `AttentionManager::signal`,
   which runs the pure attention state machine and the **suppression policy**
   (`state/policy.rs`) against the focus/foreground tuple replicated from the
   frontend.
5. On a raise it emits `pane://attention` to the frontend (ring on the pane/tab)
   and, if not suppressed, surfaces an OS notification through the
   `NotificationGate` (coalesce when many panes are raised; rate-limit bursts).

Attention has a **tiered confidence model** (`Tier`: Hook/Cli/Bel/Osc). `Hook`
is Claude Code's own hook (the v1 default); `Cli` is a raise from any `fly
notify` caller — the automations alert path (KTD-H) surfaces a non-silent
script run as `Signal { reason: Reason::Alert, tier: Tier::Cli }`, the first
non-agent attention producer. `Reason::Alert` flows end-to-end (raise → ring →
triage badge, U12/R18); `Bel`/`Osc` remain forward design. Each pane is modeled
by **two orthogonal pure state machines**: `state/lifecycle.rs` (process status)
and `state/attention.rs` (Idle→Raised→Acknowledged). Both take time/inputs as
arguments so they're tested without a running app.

### Backend modules (`src-tauri/src/`)
- `pty/` — `PtyManager` registry + `Pane`: portable-pty, one read thread per
  pane, backpressure pause/resume (watermarks), ordered reap-on-exit.
- `stream/` — `spawn_pane`, raw-byte PTY output over a Tauri `Channel` (no
  transcoding), and the pane↔attention/focus wiring + Tauri commands.
- `state/` — the two state machines + suppression policy (`policy.rs`) +
  per-pane `manager`.
- `hooks/` — the authenticated socket: `token`, `protocol` (the notify path +
  the `automation/*` request envelope, U9), `server`.
- `cli/` — `fly notify`, `fly hooks setup|teardown`, `fly automation …` (U9).
- `automations/` — cron-scheduled agent/script runs (see the module map below).
- `notify/`, `config/`, `session/`, `cwd/` (via `/proc`), `lifecycle.rs`
  (ordered shutdown — reap every pane, no zombies/orphans).
- All Tauri commands are registered in the `invoke_handler!` in `lib.rs`; the
  frontend's typed wrappers for them live in `src/ipc.ts`. Add a command in both
  places.

### Automations (`src-tauri/src/automations/`, cross-referenced U1–U12)
Cron-scheduled runs that either spawn a `claude "<prompt>"` agent pane (Agent
mode) or run a stored script with no model spend (Script mode). Data flow: a
named `fly-automation-sweep` thread ticks every 10s; a due automation is
**claimed + persisted before it runs** (R2), then dispatched off the store lock
(KTD-B). Modules:
- `model.rs` (U1) — pure domain vocabulary: `Automation`, its bounded run
  history (`RunRow`, R8), and the run-row state machine (claim/skip/close). Serde
  **camelCase** — this shape crosses the store file, the socket, and the
  dashboard, so it is the single wire contract.
- `schedule.rs` (U2) — cron/timezone math (croner), the 5-min min-gap clamp (R1).
- `store.rs` (U3) — the write-through **mutex-authority** store (KTD-B): the
  in-memory map is authoritative, flushed atomically per mutation; `StoreHealth`
  tracks corruption (`.bad.bak` rename, R6) and flush failures for the dashboard.
- `mod.rs` (U4) — `AutomationManager` (create/pause/resume/delete/manual-run),
  the sweep, startup recovery, ordered shutdown; the `list_automations` command
  (U10) and `AutomationsDashboard` DTO. `AUTOMATION_CHANGED_EVENT`
  (`automation://changed`, payload = the automation id) fires after every
  mutation so the dashboard refetches.
- `script.rs` (U5) — the script runner (interpreter enum, timeout, output
  classification → alert vs silent). An alert-classified run hands off through
  the injected `AlertSink` seam (U6 wires the real one).
- `alerts.rs` (U6) — alert surfacing. `AlertsLog` owns the sanitized
  append-only `automation-alerts.log` (R16: `notify::sanitize_*` strips control
  chars incl. newlines at write time, so a script can't forge a log line;
  64 KiB tail-truncated on startup), a bounded **pending queue** for alerts
  arriving before the sink pane exists (R17), and the **sink registry**
  (`register_sink`/`clear_sink_if`). `lib.rs`'s `set_alert_sink` closure (on the
  reaper thread; only this lock + the log file, never the store lock — KTD-B)
  appends then rings the sink pane via `raise_alert` (`Signal { Alert, Cli }` →
  `emit_attention`, R18) or queues + emits `automation://alert-pending`. The
  frontend single-flights a background "Automations" tab that `tail -f`s the log
  and calls the `register_alert_sink` command, which drains the backlog.
- Agent dispatch (U7/U7.5/U8) links run↔pane atomically in `stream::spawn_pane`
  (threading `automation_run_id`), spawns a background ephemeral tab
  (`App.svelte` `handleAgentRun` + `lib/automation-panes.ts`), and closes the run
  on the agent's Stop / pane-exit / 30-min deadline. The **R22 recursion gate**
  blocks an automation-spawned pane from creating or running automations.
- CLI (`cli/automation.rs`, U9): read ops (`list`/`show`/`runs`) read the store
  file directly (work outside a pane); mutating ops (`create`/`pause`/`resume`/
  `run`/`delete`) post over the hook socket (token-validated, origin-stamped,
  R22-gated).
- Dashboard panel (U10): `lib/automations.ts` is the pure view-model
  (`automationsToRows` — sort next-run asc / paused last, mirroring the CLI —
  plus `humanSchedule`/`relativeTime`); `HomeView.svelte` renders it read-only
  below the agent list, with the R6 store-health warning row.

### Frontend (`src/`)
- `App.svelte` — orchestrates workspaces, tabs, and the split tree; owns
  attention/cwd state, debounced session persistence (~800ms), and overlay
  (hotkey menu / destructive-confirm) wiring.
- `lib/layout.ts` — **pure split-tree model**. Leaves render flat and keyed, so
  splitting/resizing never unmounts a pane (which would respawn its agent). Leaf
  keys are stable and also key the scrollback files — preserve this invariant.
  `App.svelte` renders every pane across **all** workspaces/tabs (hiding inactive
  ones) so switching never unmounts/respawns an agent — same invariant.
- `lib/workspaces.ts` — **pure workspace/tab model** (mirrors `layout.ts`): a
  workspace is a named collection of tabs; helpers (`tabDisplayTitle`,
  `closeTabIn`, `deleteWorkspaceFrom`, `flattenRaised`) take id factories so
  they're tested without an app.
- `lib/keymap.ts` — the leader-key model (tmux-style: default Ctrl-A, then a
  command key; everything else passes through to the PTY). `BINDINGS` is the
  single source of truth shared by `dispatch()`, the hotkey menu, and the
  command palette, so they cannot drift.
- `lib/Terminal.svelte` — embeddable xterm leaf; subscribes to `pane://attention`.
  Terminal font size comes from config (`config.fontSize`, default 15).
- `lib/Sidebar.svelte` — collapsible cmux-style workspace tree (workspaces ▸
  named tabs); `lib/ControlBar.svelte` — slim top bar (sidebar toggle +
  breadcrumb + pane controls).
- `lib/{config,serialize}.ts` (`serialize.migrateSession` upgrades old sessions
  into the workspace shape), `lib/HotkeyMenu.svelte` (passive cheat-sheet).
- `lib/CommandPalette.svelte` + `lib/palette.ts` — type-to-run command palette
  on `leader p`: every `BINDINGS` action (so it can't drift) plus live
  jump-to-workspace/tab navigation. Unlike the cheat-sheet it takes DOM focus,
  so `App.focusActivePane()` hands focus back to the active pane on close.

## Conventions

- Code is cross-referenced to the design by ID — **KTD\<n\>** (key technical
  decision) and **R\<n\>**/**U\<n\>** (requirement/unit) appear in doc comments
  and tie back to `docs/plans/`. When changing behavior, keep the referenced IDs
  accurate. Match the surrounding style; modules are heavily doc-commented.
- Behavior-bearing units ship with tests (Rust state machines are test-first and
  pure; frontend has vitest for layout/keymap).
- Commits: conventional, with a `Co-Authored-By: Claude` trailer.
