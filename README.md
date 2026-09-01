# fly

A Linux desktop **terminal for AI coding agents**: real PTY-backed panes, tabs
and splits (one agent per pane), and an attention indicator plus OS notification
the moment an agent needs you. v1 wires **Claude Code** as the attention source.

**Stack:** Rust backend (`fly core`, a headless process serving a Unix control
socket) · [Electron](https://electronjs.org) shell (the desktop window) ·
[Svelte 5](https://svelte.dev) (Vite / TypeScript frontend) ·
[xterm.js](https://xtermjs.org) terminal panes. **Linux only.** (The Tauri
shell fly started on, and its best-effort macOS build, were retired on
2026-08-27; tag `tauri-shell-final` if you ever need them.)

> The full design lives in [`docs/plans/`](docs/plans/README.md) (indexed by ID);
> the primary contributor guide is [`CLAUDE.md`](CLAUDE.md). This README is the
> orientation layer on top of both.

## Why

Running several coding agents at once means babysitting several terminals: you
can't tell which one is blocked on you without tabbing through them. fly makes
each agent a first-class pane, watches all of them, and rings the one that needs
attention, so you can run a fleet and only look when there's something to do.

## Features

- **Panes, tabs, workspaces, splits**: one agent per pane, arranged in a pure
  split tree. Leaves render flat and keyed, so splitting or resizing never
  unmounts a pane (which would respawn its agent). Workspaces group named tabs.
- **The tmux session substrate** (opt in with `substrate: "tmux"` in config):
  every pane becomes a marked session on a fly-owned tmux server, so **agents
  outlive the app**: quitting detaches, restarting adopts the same child
  processes and replays scrollback. `leader t` opens the focused session in a
  real terminal for native-latency typing.
- **The attention pipeline**: a Claude Code hook (`fly notify`) reaches the
  running app over an authenticated Unix socket; a pure state machine decides
  whether to ring the pane/tab and, unless suppressed, raise an OS notification
  (coalesced across panes, rate-limited on bursts).
- **Resume & handoff**: each pane's last session is durably recorded, so a
  crash can offer to resume every agent, and a stale pane can hand its previous
  session to a fresh `claude` in a split alongside (`leader f` quick / `leader F`
  guided). Resumed agents replay their captured launch flags; when the flags
  are unknown, fly resumes in Claude's default permission mode
  (`resumeDefaultArgs` in config sets a different floor).
- **Session attribution**: precise, per-pane session identity even when several
  `claude` sessions share one working directory (see below).
- **Automations**: cron-scheduled agent or script runs (`fly automation …`),
  with alert surfacing back into a pane and per-automation model/effort
  defaults. Agent runs dispatch **headless** by default: a backend-owned
  `claude -p` child, no pane or tab, one prompt in and one captured result out;
  `create --paned` opts an automation back into the older ephemeral-tab path,
  which runs in a dedicated Automations workspace.
- **Automation dependencies**: `create --after <id> [--within <dur>]` chains
  one automation behind another: the dependent's cron occurrence fires only
  against a fresh, successful, not-yet-consumed upstream run, waits out the
  window otherwise, and then records an honest withheld row naming why,
  never a green row over stale data.
- **Agent peer messaging**: `fly agents` lists the live agent roster and `fly
  send <pane> <message…>` delivers a provenance-framed message into another
  agent's pane. Receiving is off by default and opt-in per pane, by you, from
  the dashboard row's "peers" toggle; no agent can opt itself in.
- **Monitors**: an automation flavor for parked experiments: sparse re-checks
  run headless (a backend-owned `claude -p`, no pane or tab), deliver one
  fenced PASS/FAIL verdict, and retire the monitor. A FAIL writes a durable
  failure bundle and the dashboard offers a one-action recovery pickup
  (`fly automation create --monitor --not-before …`; the check prompt carries
  the verdict-block contract, so any agent that can run the CLI can register
  one).
- **The feed**: a loopback HTTP surface (bearer-token auth) for an external
  local consumer: an SSE roster of agents + automations (including monitor
  verdicts), per-agent latest reply / pending question / conversation tail,
  and a guarded input route for answering an agent remotely.
- **Phone screenshot drop**: a page served over your tailnet that sends a
  screenshot plus a caption from your phone straight into a live agent pane
  (`POST /drop`): the image is stored `0600` and the agent is asked to read it.
  Reachability is `tailscale serve` in front of the same loopback port; fly
  adds no bind. Requires a one-time operator setup; see
  [`docs/notes/2026-07-24-phone-drop-live-check.md`](docs/notes/2026-07-24-phone-drop-live-check.md)
  ("Operator setup").
- **Agent dashboard**: a "home" view (`leader d`) of every agent's status
  (waiting / working / idle / running), plus live plan-usage gauges.

## Session attribution

*The problem.* A pane's session id was captured by an always-on poll that picked
the newest-mtime transcript in `~/.claude/projects/<encoded-cwd>/`. That key is
the **directory alone**, nothing pane-specific. Two `claude` sessions in one
repo therefore overwrote each other's per-leaf resume records, and quick
launch / crash-resume acted on whichever session wrote last. In practice: quick
launch from pane B silently picked up pane A's session.

*The fix.* Attribute each pane's session to its **own** pane at session birth:

- A capture-only **`SessionStart` hook** (`fly notify --claude --capture`,
  installed by `fly hooks setup`) stamps a pane-precise session id over the
  authenticated socket, without raising any attention.
- The capture source is **trust-ranked `Poll < Hook < Pick`**. A precise capture
  (hook) can't be clobbered by the cwd-level poll; a human **pick** is the
  highest authority and a forgeable hook can never override or silently clear it
  (a divergent hook only flags a re-pick prompt).
- The poll **abstains** rather than guesses when more than one live session
  shares a cwd: silent-wrong becomes capture-nothing.
- When launch is still ambiguous, a **pick-list** shows the cwd's candidate
  sessions (last activity + a recent-turn snippet + provenance); the choice is
  remembered.
- **Corroborate-then-remember**: an unattended, bypass-permissions quick handoff
  fires zero-prompt only against a remembered pick; an uncorroborated hook/poll
  target lists once, then the persisted pick keeps every later launch
  zero-prompt.
- `leader g` **resets** a leaf's attribution and forces a re-pick, the escape
  valve for a stale or mis-attributed id.

*Validated live.* The `SessionStart` contract this rests on was confirmed
empirically against Claude Code 2.1.200: the hook inherits `FLY_PANE_TOKEN`,
carries `session_id` / `transcript_path` / `cwd` / `source`, and `/clear`
rotates to a distinct id (so the hook→hook rotation holds). Note that a plain
`claude` in an untrusted directory may sit at the folder-trust prompt without
flushing a transcript, so the **resume store, not the transcript file, is the
reliable capture signal**.

## Build & run

The **shipped product is the Electron shell**: a thin Electron window that
spawns (or adopts) a headless `fly core` backend and talks to it over a
same-uid Unix control socket (`docs/core-protocol.md`). Build and package it:

```bash
pnpm install     # every JS dep: frontend and shell (one pnpm workspace)
pnpm build:deb   # vite build → cargo build --release → electron-builder
                 # → electron/dist-el/fly-electron-shell_<ver>_amd64.deb
```

Install the built `.deb` (`sudo apt install ./fly-electron-shell_<ver>_amd64.deb`;
it installs as package `fly`) for a standalone `/usr/bin/fly` + launcher,
independent of the source tree. After installing, run **`fly hooks setup`** once
to install the Claude Code hooks (attention + the `SessionStart` capture hook);
`fly hooks teardown` removes only fly's hooks.

For the Electron dev loop and running a dev build **alongside** the installed
release (isolated state under `FLY_APP_NAME`), see `CLAUDE.md` → Commands.

## Testing

```bash
pnpm check                                                          # svelte-check (types)
pnpm test:unit                                                     # vitest (frontend)
cargo test --offline --manifest-path core/Cargo.toml         # Rust (state machines, socket auth, …)
```

## Repository layout

- `core/src/` is the Rust backend: `pty/`, `stream/`, `state/` (pure state
  machines), `hooks/` (the socket **security boundary**, with its own scoped
  `CLAUDE.md`), `session/` (layout + resume/handoff/attribution), `automations/`
  (incl. monitors + the headless check runner), `feed/` (the loopback HTTP
  surface), `peer/` (agent-to-agent messaging), `substrate/` (the tmux session
  substrate), `control/` (the `fly core` control socket + the one command
  table, `registry.rs`), `backend.rs` (the backend builder `fly core` boots
  through), `usage/`, `cli/`.
- `electron/` is the shipped Electron shell: main process, preload bridge,
  and the JS half of the control-socket frame codec (`protocol.js`), plus the
  `.deb` packaging (`deb/`).
- `src/` is the Svelte frontend: `App.svelte` orchestrator, pure view-models
  (`lib/layout.ts`, `lib/home.ts`, `lib/session-picker.ts`, …), the
  `Terminal.svelte` xterm leaf, and `lib/transport.ts`, the one seam that
  speaks to the shell (the Electron preload bridge, `window.fly`).
- `docs/plans/` is the full design, cross-referenced to the code by ID
  (`KTD<n>` / `R<n>` / `U<n>`); `docs/notes/` holds one-off evaluations and
  live-check records; `docs/core-protocol.md` is the control-socket wire
  contract.

## The CLI

The same binary is the `fly` CLI, the headless backend (**`fly core`**, what
the Electron shell spawns and drives), and, run bare or as `fly resume`,
the launcher that execs the installed desktop shell. Inside a pane, `fly
notify` talks to the running app; `fly hooks setup|teardown` manages the Claude
Code hooks; `fly substrate-event` (the tmux-hook endpoint) and `fly
substrate-pipe` (the per-pane output consumer under the tmux substrate) are
internal, not for human use; `fly automation
<create|update|list|show|runs|pause|resume|run|delete>`
manages cron-scheduled runs (`create --monitor --not-before …` registers a
monitor; `update` patches a stored automation in place, keeping its id and run
history; `list`/`show`/`runs` are monitor-aware). `fly agents` lists the live
agent roster and `fly send <pane> <message…>` delivers into another agent's
pane (opt-in per pane, from the dashboard).

## Security

fly has two real trust boundaries: the token-authenticated hook socket
(`core/src/hooks/`) and the bearer-token feed listener (`core/src/feed/`).
To report a vulnerability, see [`SECURITY.md`](SECURITY.md).

## License

[MIT](LICENSE).
