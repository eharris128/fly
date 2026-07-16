# fly

A Linux desktop **terminal for AI coding agents**: real PTY-backed panes, tabs
and splits (one agent per pane), and an attention indicator plus OS notification
the moment an agent needs you. v1 wires **Claude Code** as the attention source.

**Stack:** [Tauri v2](https://tauri.app) (Rust backend) · [Svelte 5](https://svelte.dev)
(Vite / TypeScript frontend) · [xterm.js](https://xtermjs.org) terminal panes.

> The full design lives in [`docs/plans/`](docs/plans/README.md) (indexed by ID);
> the primary contributor guide is [`CLAUDE.md`](CLAUDE.md). This README is the
> orientation layer on top of both.

## Why

Running several coding agents at once means babysitting several terminals: you
can't tell which one is blocked on you without tabbing through them. fly makes
each agent a first-class pane, watches all of them, and rings the one that needs
attention — so you can run a fleet and only look when there's something to do.

## Features

- **Panes, tabs, workspaces, splits** — one agent per pane, arranged in a pure
  split tree. Leaves render flat and keyed, so splitting or resizing never
  unmounts a pane (which would respawn its agent). Workspaces group named tabs.
- **The attention pipeline** — a Claude Code hook (`fly notify`) reaches the
  running app over an authenticated Unix socket; a pure state machine decides
  whether to ring the pane/tab and, unless suppressed, raise an OS notification
  (coalesced across panes, rate-limited on bursts).
- **Resume & handoff** — each pane's last session is durably recorded, so a
  crash can offer to resume every agent, and a stale pane can hand its previous
  session to a fresh `claude` in a split alongside (`leader f` quick / `leader F`
  guided).
- **Session attribution** — precise, per-pane session identity even when several
  `claude` sessions share one working directory (see below).
- **Automations** — cron-scheduled agent or script runs, with alert surfacing
  back into a pane (`fly automation …`), a dedicated Automations workspace,
  and per-automation model/effort defaults.
- **Monitors** — an automation flavor for parked experiments: sparse re-checks
  run headless (a backend-owned `claude -p`, no pane or tab), deliver one
  fenced PASS/FAIL verdict, and retire the monitor. A FAIL writes a durable
  failure bundle and the dashboard offers a one-action recovery pickup
  (`fly automation create --monitor --not-before …`, taught to agents by the
  `fly-monitor-handoff` skill).
- **The feed** — a loopback HTTP surface (bearer-token auth) for an external
  local consumer: an SSE roster of agents + automations (including monitor
  verdicts), per-agent latest reply / pending question / conversation tail,
  and a guarded input route for answering an agent remotely.
- **Agent dashboard** — a "home" view (`leader d`) of every agent's status
  (waiting / working / idle / running), plus live plan-usage gauges.

## Session attribution

*The problem.* A pane's session id was captured by an always-on poll that picked
the newest-mtime transcript in `~/.claude/projects/<encoded-cwd>/`. That key is
the **directory alone** — nothing pane-specific. Two `claude` sessions in one
repo therefore overwrote each other's per-leaf resume records, and quick
launch / crash-resume acted on whichever session wrote last. In practice: quick
launch from pane B silently picked up pane A's session.

*The fix.* Attribute each pane's session to its **own** pane at session birth:

- A capture-only **`SessionStart` hook** (`fly notify --claude --capture`,
  installed by `fly hooks setup`) stamps a pane-precise session id over the
  authenticated socket — without raising any attention.
- The capture source is **trust-ranked `Poll < Hook < Pick`**. A precise capture
  (hook) can't be clobbered by the cwd-level poll; a human **pick** is the
  highest authority and a forgeable hook can never override or silently clear it
  (a divergent hook only flags a re-pick prompt).
- The poll **abstains** rather than guesses when more than one live session
  shares a cwd — silent-wrong becomes capture-nothing.
- When launch is still ambiguous, a **pick-list** shows the cwd's candidate
  sessions (last activity + a recent-turn snippet + provenance); the choice is
  remembered.
- **Corroborate-then-remember**: an unattended, bypass-permissions quick handoff
  fires zero-prompt only against a remembered pick; an uncorroborated hook/poll
  target lists once, then the persisted pick keeps every later launch
  zero-prompt.
- `leader g` **resets** a leaf's attribution and forces a re-pick — the escape
  valve for a stale or mis-attributed id.

*Validated live.* The `SessionStart` contract this rests on was confirmed
empirically against Claude Code 2.1.200: the hook inherits `FLY_PANE_TOKEN`,
carries `session_id` / `transcript_path` / `cwd` / `source`, and `/clear`
rotates to a distinct id (so the hook→hook rotation holds). Note that a plain
`claude` in an untrusted directory may sit at the folder-trust prompt without
flushing a transcript, so the **resume store — not the transcript file — is the
reliable capture signal**.

## Build & run

System deps (Tauri / WebKitGTK on Ubuntu):

```bash
sudo apt install libwebkit2gtk-4.1-dev build-essential libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf
```

Then:

```bash
pnpm install                     # frontend deps
pnpm tauri dev                   # run the app (Vite dev server + cargo run)
pnpm tauri build --bundles deb   # standalone .deb → src-tauri/target/release/bundle/deb/
```

Install the built `.deb` (`sudo apt install ./fly_<ver>_amd64.deb`) for a
standalone `/usr/bin/fly` + launcher, independent of the source tree. After
installing, run **`fly hooks setup`** once to install the Claude Code hooks
(attention + the `SessionStart` capture hook); `fly hooks teardown` removes only
fly's hooks.

To iterate on a dev build **alongside** an installed release without the two
clobbering each other's state, use `pnpm flavor:dev` (a distinct Tauri identity
and isolated on-disk state under `FLY_APP_NAME=fly-dev`).

## Testing

```bash
pnpm check                                                          # svelte-check (types)
pnpm test:unit                                                     # vitest (frontend)
cargo test --offline --manifest-path src-tauri/Cargo.toml         # Rust (state machines, socket auth, …)
```

## Repository layout

- `src-tauri/src/` — Rust backend: `pty/`, `stream/`, `state/` (pure state
  machines), `hooks/` (the socket **security boundary** — has its own scoped
  `CLAUDE.md`), `session/` (layout + resume/handoff/attribution), `automations/`
  (incl. monitors + the headless check runner), `feed/` (the loopback HTTP
  surface), `usage/`, `cli/`.
- `src/` — Svelte frontend: `App.svelte` orchestrator, pure view-models
  (`lib/layout.ts`, `lib/home.ts`, `lib/session-picker.ts`, …), and the
  `Terminal.svelte` xterm leaf.
- `docs/plans/` — the full design, cross-referenced to the code by ID
  (`KTD<n>` / `R<n>` / `U<n>`).

## The CLI

The same binary is both the desktop app and the `fly` CLI. Inside a pane, `fly
notify` talks to the running app; `fly hooks setup|teardown` manages the Claude
Code hooks; `fly automation <create|list|show|runs|pause|resume|run|delete>`
manages cron-scheduled runs (`create --monitor --not-before …` registers a
monitor; `list`/`show`/`runs` are monitor-aware).
