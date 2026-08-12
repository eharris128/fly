# Proposal: Electron/Chromium shell migration — 2026-08-12-002

**Status**: in progress — U1 + U2 landed 2026-08-12 (transport decision
resolved to the control socket per recommendation). U1:
`src-tauri/src/control/` + `fly core` + `docs/core-protocol.md`. U2:
`control/registry.rs` — 42 of the 45 commands ported name-identically onto
the seam over the real managers (config/PTY+substrate/attention/coalescers;
shared-body extraction where a command holds logic: `pane_activity_snapshot`,
`attach_pane_now`, `dashboard_snapshot`, `read_bundle_for`,
`attention_event_payload`); the 3 shell-coupled stragglers (`spawn_pane`,
`register_alert_sink`, `get_launch_mode`) answered a named-U3 error. U3
(landed): `spawn_pane` extracted into shell-agnostic `stream::spawn_pane_with`
(token adopt/issue, automation linking, coalesced output, ordered exit
teardown — one body, both shells); pane output rides the 0x02 binary frames,
keystrokes ride 0x03 down (`fly core` wires the `PaneInputHandler` to
write + attention-clear like `pty_write`); `pane://exit`/`pane://attention`
fan out via the shared payload builders; `get_launch_mode` +
`register_alert_sink` ported (core resolves its flavor's clean-exit marker at
boot). U3.5 (landed):
`src-tauri/src/backend.rs` — the whole Tauri setup wiring (hook-server
dispatch, ask/peer/automation/substrate handlers, automations subsystem +
sweep + alert surfacing, feed listener) extracted into
`backend::build_backend(seams)` against two injected seams (events sink +
banner); `lib.rs` setup shrinks to seam construction + `.manage()` of the
returned pieces, and `fly core` boots the identical backend (banner via
`notify-send` pending KTD8's notify-rust) — live-verified: a real
`fly notify` from a core-spawned pane raised `pane://attention` through the
authenticated hook socket to a control client. U4 (landed): `electron/` —
thin main (window, `requestSingleInstanceLock` per flavor userData,
spawn-or-adopt `fly core` with crash-restart + reconnect), the JS frame
codec (`protocol.js`, edited with `docs/core-protocol.md`), a fully
sandboxed renderer whose only surface is the preload's `window.fly`
(invoke/onEvent/onPaneOutput/paneInput — the main process owns the socket,
KTD4), and a throwaway `probe.html` that live-verified spawn + byte
streaming in a window. Dev caveat: the repo box lacks the Chromium SUID
helper, so dev runs use `--no-sandbox` (packaging restores it). U5 (landed):
`src/lib/transport.ts` is the frontend's one transport seam — invoke/listen/
pane-output-sink/window-close, Tauri or `window.fly`, detected at runtime;
`ipc.ts`/`config.ts`/`serialize.ts`/`main.ts`/`Terminal.svelte`/`App.svelte`
route through it (the only structural change: `spawn_pane`'s Channel becomes
an `OutputSink` — Electron subscribes by pane id post-spawn with a bounded
pre-subscription buffer). Bridge invokes JSON-round-trip their args (Svelte 5
`$state` proxies fail Electron's structured clone; Tauri always
JSON-serialized, so semantics are preserved exactly). Quit-confirm flows
through `fly:close-request`/`close-now`. **Live-verified: the full fly UI —
sidebar, dashboard, automations panel, real OAuth usage gauges — running on
Chromium against the headless core** (`FLY_SHELL_URL=http://localhost:1420`,
flavor `fly-el`). U6 (in progress): ordered shutdown for `fly core` landed
2026-08-12 — the lifecycle.rs teardown sequence extracted into
`backend::ordered_shutdown` (one sequence, both shells; lifecycle.rs is now a
try_state adapter), triggered by the new `core/shutdown` control command or
SIGTERM/SIGINT (both land on one atomic flag; the run loop polls it, so the
ack flushes before teardown), and the Electron shell's before-quit drives
`core/shutdown` first (spawned *or* adopted core — quitting fly quits the
backend) with SIGTERM fallback and a 10 s SIGKILL deadline. Live-verified
both triggers: exit 0, clean-exit marker written, ordered log line.
KTD6 detach/adopt **live-verified under the
Electron shell** (2026-08-12, fly-el on `substrate: "tmux"`): quit → ordered
core shutdown → session survived on the fly-el tmux server; relaunch →
spawn-or-adopt → same pane pid, scrollback replayed (pre-restart marker
visible), pane resized on first show, keystrokes round-trip. Found+fixed en
route: an *empty* dashboard held no DOM focus, so its Esc/digit keys were
dead (pre-existing, both shells — HomeView now focuses its container when no
row exists). U6 parity checklist (2026-08-12, all
live on fly-el/Electron/tmux, driven over CDP + the control socket):
**attention end-to-end** — a real `claude` REPL's Stop hook and a
borrowed-token `fly notify question --claude` both raised `pane://attention`
(tier hook) through the *adopted* pane's re-registered token; focused →
acknowledged, unfocused → raised (focus replication works); full UI: pane
ring + "waiting for you" badge + tab/workspace dots + bell count. Banner
seam = notify-send (present; not visually captured — Wayland compositor
owns it). **feed** — healthz, silent 401, authed SSE roster with live agent
row + automations projection, unauthenticated drop page 200; fly-el moved
to feed port 4941 (fly keeps 4939). **peer** — `fly agents` answers over the
hook socket. **automations** — create/update/run over the socket from a
pane, failed and alert-classified closes, sanitized alerts log, sink
workspace + Automations tab provisioned, dashboard panel row. **resume** —
hook-ranked pane-precise capture into resume.json (sessionId + argv) for a
real agent. **chords** — leader d (dashboard), leader t (real terminal
attached the session, listed by `tmux list-clients`), Esc overlays.
**quit-confirm** — busy agent + WM close → "1 agent is still working. Quit
anyway?" over `fly:close-request`; Esc cancels; idle close persists and
quits clean. Not exercised: handoff chords end-to-end (transport-proven;
frontend identical), peer send/drop delivery (shared deliver_with_guards
path), KTD8 notify-rust (deferred to U7 — notify-send serves the seam).
Remaining: U6 perf gate + Wayland pass, U7 packaging/cutover, U8
simplifications.
**Grounds**: `docs/notes/2026-08-12-electron-engine-probe.md` (same-box probe:
flood main-thread 63 % → 11.7 %, echo 99 → 53 ms p50; ~50 ms xterm.js pipeline
floor persists on any engine) and
`docs/notes/2026-08-11-webkitgtk-engine-floor.md` (the A/B/C fork).

## What this buys — and what it doesn't

Buys (measured, this box): the WebKitGTK main-thread wall is *gone* (many
streaming panes at ~12 % of a core instead of pegged), echo halves, unfocused
throttling nearly disappears, and the webview console becomes real devtools.
Does **not** buy: native typing feel — the ~50 ms xterm.js frame/batching
floor rides along. `leader t` native attach remains the answer for
typing-heavy stretches; this migration makes the *ambient* experience (N live
agents, dashboards, streams) stop fighting the render thread.

## Shape: three processes, one seam

Today: Tauri app = one process (Rust) hosting the webview, frontend talking
through ~48 `invoke` commands, 8 `pane://`/`automation://` events, and one
byte `Channel` (PTY output), all defined at the `lib.rs` invoke_handler /
`src/ipc.ts` seam. Proposal:

1. **`fly core`** — a new role of the *same* Rust binary (beside CLI and,
   during transition, the Tauri app): the entire `fly_lib` backend running
   headless, serving the existing command/event/stream surface over a local
   control socket. Everything fly *is* — hooks socket, attention machines,
   substrate, automations, feed, peer, session/resume — runs here, unchanged.
2. **Electron main** — a thin shell: window management, single-instance lock,
   spawns/adopts the core, forwards its lifetime.
3. **Renderer** — the existing Svelte/Vite/xterm.js frontend, byte-identical
   in spirit; only the transport under `ipc.ts` changes.

### Key technical decisions (proposed)

- **KTD1 — the seam is `ipc.ts`, and names don't change.** One wire-protocol
  doc; the 48 commands and 8 events keep their exact names and serde shapes
  (they're already the wire contract — camelCase serde, typed wrappers in one
  file plus `lib/{config,serialize}.ts`). Backend handlers port mechanically
  from `#[tauri::command]` fns to a core dispatch match — they are already
  thin wrappers over `fly_lib`.
- **KTD2 — the security boundary does not move.** Nothing crosses to
  TypeScript: hook socket, feed listener, drop route, automations, secret
  scrubbing all stay in Rust, in-process with the core. The new control
  socket is same-uid only (0700 runtime dir + `SO_PEERCRED` uid check — the
  `hooks/` discipline, reused).
- **KTD3 — PTY bytes go binary end-to-end.** Length-prefixed frames on the
  core socket, `ArrayBuffer` through the preload bridge. This *removes* the
  old KTD3 eval quirk (tauri re-encoding <1 KiB chunks as JSON number arrays
  at ~3.4× wire cost) rather than porting it. The coalescer survives but its
  visible-deadline tuning gets re-measured on Chromium (U8).
- **KTD4 — renderer hardened opposite of the spike.** `contextIsolation` on,
  `nodeIntegration` off, Chromium sandbox on; the preload exposes only the
  typed bridge. (The spike's lab-bench settings must not leak into the
  product.)
- **KTD5 — side-by-side via the existing flavor mechanism.** The Electron
  build develops as `FLY_APP_NAME=fly-el`: own config/session dirs, own hook
  socket, own tmux server. Zero new isolation code; the stable Tauri app
  keeps running beside it all through U6.
- **KTD6 — the tmux substrate is the cutover mechanism.** Sessions outlive
  the shell. Final migration on the `fly` flavor is: quit Tauri fly
  (detaches sessions), launch Electron fly (adopts them — same pids, tokens
  re-registered, scrollback replayed). **Zero agent downtime at cutover**,
  and instant rollback by the reverse move. This is the substrate's lifecycle
  inversion paying for itself.
- **KTD7 — single-instance = Electron's `requestSingleInstanceLock` per
  flavor**, plus the existing never-steal-a-live-socket bind discipline on
  the core socket. Current focus-existing-window behavior replicated.
- **KTD8 — OS notifications move to Rust-native** (`notify-rust` over DBus —
  v1 is Linux-only), removing the `tauri-plugin-notification` dependency
  from the core rather than re-plumbing it through the Electron shell. The
  `NotificationGate` logic is untouched. (`tauri-plugin-store` /
  `single-instance` likewise retire with the shell.)
- **KTD9 — the Tauri shell stays buildable during transition** (feature
  flag), retired only after the Electron build has soaked as the daily
  driver. Accepted costs, eyes open: ~100 MB bundle vs today's 4.5 MB .deb,
  higher RSS (renderer+gpu processes), and the Electron update/CVE cadence
  as a standing chore.

## Units

- **U1 — wire protocol + `fly core`.** The protocol doc (envelope,
  request/response ids, event fan-out, binary stream frames) and the core
  socket server scaffold behind the new subcommand. Includes the same-uid
  authz and the never-steal bind.
- **U2 — command port.** The 48 commands move from `invoke_handler` to core
  dispatch (mechanical; each already delegates to `fly_lib`). Tauri shell
  keeps working throughout (KTD9) — during transition the handlers are
  shared, shell-agnostic fns.
- **U3 — events + byte streams.** `emit_attention` & friends fan out over
  the core socket; PTY output rides the binary frames (KTD3); backpressure
  semantics preserved (the pause/resume watermarks live below this seam and
  don't change).
- **U3.5 — the full headless host** (carved out of U3 during execution —
  2026-08-12: U3 landed the events/byte-stream seam and pane lifecycle; what
  remains to make `fly core` a *complete* backend is extracting lib.rs's
  setup wiring into a shell-agnostic builder: the hook server's dispatch
  closure (attention/notify/resume-capture/feed bumps), the ask/peer/
  automation/substrate handlers, the automations manager + sweep + alerts
  with their event emitters, and the feed listener. Until then the U3 core
  spawns fully functional panes whose env points at the stable hook-socket
  path, but no hook server answers there; automations/feed commands answer a
  named-U3.5 error.)
- **U4 — Electron shell.** `main` + `preload`: window, single-instance
  (KTD7), core spawn/adopt with crash-restart, `fly-el` flavor wiring, dev
  loop (Vite dev server + electron, mirroring `pnpm flavor:dev`).
- **U5 — `ipc.ts` over the bridge.** Same exported API, new transport;
  `frontend_log` keeps forwarding to core stderr *and* gains real devtools.
- **U6 — parity + perf gate on `fly-el`.** The live checklist: attention
  end-to-end (hook → ring → notification), substrate detach/adopt, feed +
  drop + peer, automations (headless run, alert ring, dashboard), resume/
  handoff chords, palette/keymap. Perf gate re-run in the *real* app with
  the existing probes: echo p50 ≤ 60 ms focused, 5-pane flood renderer main
  thread ≤ 20 %, typing-under-load < 2×. Miss → stop and reassess, Tauri
  still primary.
- **U7 — packaging + cutover.** electron-builder .deb (same `/usr/bin/fly`
  + launcher story — the binary inside is the core; the CLI role is
  unchanged), then the KTD6 detach→adopt cutover on the `fly` flavor.
- **U8 — post-cutover simplifications, measured not assumed.** Candidates
  the probe suggests Chromium makes unnecessary: `mirrorUnfocused` 2 Hz DOM
  snapshots (visible-unfocused panes may just render live), the KTD6 WebGL
  disposal-on-hide, the hidden-pane 250 ms coalesce deadline. Each removed
  only against a fresh measurement; each is its own small change.

## Effort & sequencing (rough, honest)

U1–U3 are the bulk (the seam is clean but wide): ~3–5 focused days. U4–U5:
~2–3 days. U6 checklist + soak: ~2 days plus daily-driver time. Total ≈ two
weeks part-time, with the app fully usable (Tauri) the whole way. Nothing in
U1–U5 blocks other fly work; the seam extraction (shared shell-agnostic
handlers) is even a standalone code-quality win if the migration stalls.

## Risks

- **Two shells in the tree for a while** — mitigated by KTD9's feature flag
  and shared handlers (no logic forks, only transport).
- **Electron treadmill** — version pinning + a quarterly bump chore; accepted
  as the price of Chromium.
- **Wayland**: Electron's ozone/Wayland path has its own quirks (the probe
  ran X11). U6 must include a `--ozone-platform=wayland` pass and an X11
  fallback documented like today's `GDK_BACKEND=x11`.
- **The 50 ms floor disappoints anyway** — expectation set at the top: this
  is a scaling + 2× echo fix. If in-window typing must reach native, that's
  option C (native surface) and a different proposal.
- **macOS best-effort target** (`docs/macos-build.md`) actually *improves*:
  Electron's mac story is more beaten-path than Tauri+WKWebView, but it's
  out of scope here.

## Open decisions (need your call)

1. **Transport: control socket (recommended) vs napi-rs embed.** The socket
   keeps process isolation, keeps the core headless-capable (a future remote
   /thin-client frontend — the feed already leans this way), and survives
   renderer crashes; the embed has marginally lower IPC latency but couples
   the Rust build to node ABI and puts fly_lib inside Electron's process. I
   recommend the socket; latency is nowhere near the bottleneck (probe: the
   floor is frame cadence, not IPC).
2. **Electron version policy** — pin major + quarterly bumps, or track
   latest?
3. **Tauri retirement horizon** — one stable release after cutover, or keep
   dual-shell indefinitely as a hedge?
4. **Go/no-go itself** — U6's gate numbers are the tripwire; agree on them
   now so the decision at that point is mechanical.
