# Proposal: Electron/Chromium shell migration — 2026-08-12-002

**Status**: in progress — U1 landed 2026-08-12 (`src-tauri/src/control/` +
`fly core` + `docs/core-protocol.md`; transport decision resolved to the
control socket per recommendation); U2 next
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
