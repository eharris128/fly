---
title: "feat: tmux as the session substrate (native-typing escape hatch)"
type: feat
date: 2026-08-11
status: implemented behind the KTD10 flag (U1–U8, U10 overnight 2026-08-11→12;
  every unit live-validated on scratch servers incl. the R4 restart roundtrip —
  same child pid, token continuity, history replay, cross-instance hooks.
  Deferred, gated on the user's dev-flavor live validation: U9 retirements
  (pty path stays), the substrate default flip, the frontend kill-all quit
  variant, settings-menu toggles for mirrorUnfocused/terminal, the in-app R2
  measurement. See LIVE-CHECKLIST beside this plan.)
origin: docs/brainstorms/2026-08-11-tmux-session-substrate-requirements.md
evidence:
  - docs/notes/2026-08-11-webkitgtk-engine-floor.md   # why (engine floor proven)
  - docs/notes/2026-08-11-tmux-substrate-stage1-and-memos.md   # spikes S1–S4 + memos D1–D13
  - docs/notes/2026-08-11-gascity-tmux-reference-mining.md     # external scar tissue
---

# feat: tmux as the session substrate

## Summary

Move pane sessions out of fly-owned PTYs into a fly-managed tmux server
(per-flavor `-L` socket). fly stays the orchestrator and projection —
attention, dashboard, feed, automations, triage are untouched at their wire
contracts — while the typing surface becomes optional: a chord attaches the
focused session in a real terminal (native keystroke latency by
construction), and fly's window renders cheap mirrors instead of N full
xterm.js emulations. Sessions outlive fly: restart reattaches, never
respawns.

Why now: the 2026-08-11 engine-floor note proves the webview main thread
saturates (~63% avg, 87% busy >50%) under a few streaming panes across every
GPU/driver configuration, with WebGL active, after T1/T2/T4 all landed — and
keystrokes share that thread. The S2 spike bounds the replacement mirrors at
4–14% worst-case for the same stimulus.

## Requirements

- **R1** Typing in an attached native terminal has no fly component in the
  keystroke path (structural).
- **R2** fly window under the 5-streaming-pane protocol: webview main thread
  < 20% average, no sustained >50% stretches (protocol in the engine-floor
  note).
- **R3** The attention pipeline works end-to-end from inside tmux panes
  (S1-proven; re-verified in the live checklist).
- **R4** fly restart with N live agents: all reattached, zero respawns, zero
  lost scrollback/transcript state; quit detaches by default.
- **R5** Feed, automations, peer messaging, and drop behave identically at
  their wire contracts; delivery refusals surface (never silent drops),
  including the new unconfirmed-submit refusal.
- **R6** fly never adopts a tmux session it did not create and mark.
- **R7** Dev flavor fully isolated: own tmux server socket, own sessions.
- **R8** tmux absent or below the version floor ⇒ clear refusal with an
  install hint at startup; no degraded half-mode.
- **R9** Suppression: a raise on an externally-attached session is treated
  as focused-elsewhere (no OS notification while the user types there).
- **R10** Cold boot (server gone): the existing resume/`--resume` offer
  flow still works from the durable stores.

## Key Technical Decisions

(Resolved in the Stage-2 memos; restated here as the plan's contract. The
D-numbers map 1:1 where noted.)

- **KTD1 (D1) — Observation is subprocess polling + tmux event hooks; no
  control mode.** Lifecycle/state ride the existing 1.5 s `panes_status`
  batch (now issuing one `list-panes -F` snapshot per tick); exits arrive
  via `pane-died` hooks (with `remain-on-exit on`, re-armed after respawn)
  running a `fly` CLI op over the hook socket (authenticated per KTD12);
  attach state via `client-attached`/`client-detached` hooks (same). Activity uses per-window
  `#{window_activity}` (never `#{session_activity}` — frozen when
  detached) with an own-input discount for fly-injected writes.
- **KTD2 (D2) — Three mirror tiers.** Focused pane: live bytes via
  `pipe-pane -o` into the existing coalesce→channel→xterm.js path (scoped
  to exactly one pane). Visible-unfocused: `capture-pane -e` snapshots at
  2 Hz rendered as styled DOM (S2: ≤14% worst-case for 5). Hidden: no
  standing observation; on-demand capture. Window geometry is a mode
  switch, not one setting (tmux ≥3.3 pins detached sessions at 80×24
  otherwise): while detached, `window-size manual` + `resize-window` to
  fly's pane grid (re-driven on fly-side resize); on `client-attached`,
  flip to `latest` so the human's terminal wins (the mirror letterboxes
  to the attached grid); on last detach, back to manual + fly's grid.
- **KTD3 (D3/D7) — fly owns the server.** Spawned by fly with a scrubbed
  environment (the `CLAUDE_CODE_CHILD_SESSION`/`CLAUDECODE` strip moves to
  server spawn), `exit-empty off`, `-u`, per-flavor `-L` socket. Every
  `new-session` is preceded by the ga-h9z degraded-server preflight
  (bounded `has-session` against an unrouteable name; refuse on anything
  but a clean negative — a wedged server must never be given the chance to
  unlink its socket and orphan every session).
- **KTD4 (D4) — Identity by marked name; store is authority.** Session
  name `fly-<flavor>-<slug(leafKey)>`, slug injective and validated
  `^[a-zA-Z0-9_-]+$` (tmux target metacharacters silently misroute).
  Discovery = session store × `has-session`. Never adopt unmarked
  sessions; a marked session whose process died surfaces as an exited pane
  (never auto-killed — fly is a terminal, the final screen matters).
- **KTD5 (D5) — Delivery ladder with verified submit.** `send-keys -l`
  ≤4096 bytes else `load-buffer`+`paste-buffer -p -d`; retry on the
  transient "not in a mode"; `WakePaneIfDetached` (resize dance) as
  version insurance; every Enter-terminated delivery confirms via the busy
  indicator and re-Enters only while idle; unconfirmed ⇒ delivery refusal
  (never silent). Per-session delivery locks replace the app-wide mutex.
  The busy matcher shares `feed/screen.rs`'s picker awareness so
  busy vs dialog-waiting cannot be conflated.
- **KTD6 (D6/D11) — Attach is a focus state.** `#{session_attached}` +
  attach hooks feed the replicated focus tuple; attached-elsewhere
  suppresses like an in-window focused pane. Attach chord launches
  `config.terminal` (default `x-terminal-emulator`) on
  `tmux -L <sock> attach -t <session>`.
- **KTD7 (D7) — Observation failure defers destruction.** An unreachable
  server / failed probe is "could not tell", never "no sessions"; every
  destructive arm (orphan handling, store pruning) defers. Quit = detach;
  "quit and kill all" is the destructive-confirm variant.
- **KTD8 (D10) — Stable socket, persisted tokens.** The hook socket drops
  its PID key: `$XDG_RUNTIME_DIR/<app>/hook.sock`. Per-session CSPRNG
  tokens injected via `-e` at create and persisted in the session store; a
  restarted fly re-registers surviving sessions under their stored tokens.
  Future env *withholding* uses the two-half `env -u` +
  `set-environment -r` pattern (respawn-pane takes no env args).
- **KTD9 (D8/D9) — tmux owns history; the ring retires.** `history-limit`
  from config (default parity 10k), set globally at **server spawn** —
  it binds panes at creation, so setting it later misses existing
  sessions; per-leaf scrollback files, the 64 KiB
  tail ring, and the vte replay retire — the feed's screen fallback
  becomes a `capture-pane -e` call. Coalescer + watermarks survive only
  for the focused pipe-pane stream.
- **KTD10 (D3) — Migration flag is a build tool, not a mode.** A config
  switch `substrate: "tmux"|"pty"` exists only inside the rollout window
  so units can land incrementally; it is removed (with the portable-pty
  path) at parity. No permanently-dual substrate.
- **KTD11 (D12) — Security posture stated, not changed.** Same-uid
  processes can attach/send-keys into fly's server — formally wider than
  fd-scoped PTY writes, unchanged in substance (same-uid already reads
  config/tokens). `hooks/CLAUDE.md` gains the note; every string spliced
  into a tmux hook command is validated against the KTD4 charset.
- **KTD12 — Server-scope token for tmux-hook event reports.** A tmux
  `run-shell` hook executes in the *server's* context: no `FLY_PANE_TOKEN`
  exists there, and `TokenRegistry::validate`'s token→PaneId model has no
  slot for it. fly mints one `FLY_SUBSTRATE_TOKEN` at server spawn into the
  server's (scrubbed) environment; it authorizes exactly the event-report
  ops (`substrate/pane-died`, `substrate/attach-state` — carrying the
  session name, resolved against the store) and nothing else — not
  notify, not `automation/*`, not `peer/*`. Same constant-time compare +
  `SO_PEERCRED` + lockout discipline as pane tokens. Event reports are
  hints, not authority: fly re-verifies the claimed state against tmux
  before acting (the poll is the backstop when a hook is lost).

## Units

Build order; each lands green behind KTD10's flag until U9.

- **U1 — tmux wrapper module** (`src-tauri/src/substrate/tmux.rs` + pure
  arg-construction core): server spawn/probe (KTD3), version floor check
  (≥3.2; validated on 3.4), session create/kill/has/list, capture, the
  KTD5 send ladder, window-size drive, hook arming. Executor seam so arg
  construction and error classification are unit-tested without tmux
  (Gas City's shape); error taxonomy includes degraded-server,
  no-server-vs-empty-server, session-gone.
- **U2 — socket + token groundwork** (KTD8; independently landable first):
  stable hook socket path, token persistence in the session store,
  re-registration handshake for surviving sessions. Prepares the reattach
  path while the substrate is still portable-pty. Bind takeover is
  try-connect-before-unlink (never unlink a socket that answers — the
  ga-h9z lesson applied to our own socket); the single-instance plugin
  remains the same-flavor duplicate guard, and flavors never collide
  (per-flavor paths).
- **U3 — spawn path switch**: `stream::spawn_pane` creates a marked tmux
  session (env via `-e`, cwd, argv as the pane command), arms `pane-died`,
  starts the focused pane's `pipe-pane` ingest into the existing channel;
  attention/cwd wiring unchanged. Leaf↔session naming (KTD4 slug) lives in
  one tested place. Ingest transport: a per-focused-pane FIFO under the
  runtime dir, `pipe-pane -o 'cat > <fifo>'`, read by a fly thread into
  the existing sink; focus switch closes the old pane's pipe (bare
  `pipe-pane`) before arming the new one, and restart re-arms
  idempotently (`-o` opens only when no pipe exists). In-window
  keystrokes: write-chain flushes land as `send-keys -l` — one
  subprocess per flush on the typing path (~1–3 ms). **Measure in U5's
  pass; if it reads in felt latency, the fallback is a persistent
  hidden-attach stdin client (the S4 `script` trick, Gas City's
  productized shape) — decided by measurement, not taste.**
- **U4 — observation switch**: `panes_status` backed by one tmux snapshot
  per tick (list-panes formats: pid, dead, activity, size,
  `#{pane_current_path}` — which may retire the `/proc` cwd walk);
  lifecycle from `pane-died` events (authenticated per KTD12,
  poll-verified); activity per KTD1 with the own-input discount wired
  to the delivery paths; `foreground_pid` from `#{pane_pid}` + /proc as
  today.
- **U5 — mirror rendering**: new snapshot channel (backend capture loop at
  2 Hz for visible-unfocused panes, event-gated by the existing
  `set_visible_panes` replication) + `Mirror.svelte` styled-DOM renderer;
  `Terminal.svelte` retained for the focused pane only; focus change swaps
  tiers (dispose xterm → mirror, and back — never unmount the component).
  The focus-gain splice is specified, not improvised: arm `pipe-pane`
  FIRST (fly buffers the stream), bounded `capture-pane -e -S -<N>`
  replay into xterm (N ≈ 2k lines — reveal speed over completeness),
  then flush the buffered stream; the overlap can duplicate a partial
  row once, accepted (same class as any terminal redraw). Honest
  limitation, stated in docs: in-window scrollback for a
  previously-unfocused pane truncates to the replay bound — the full
  history lives in tmux (attach, or capture on demand). R2 and the
  send-keys typing-latency check (U3) measured here.
- **U6 — delivery routes on the ladder** (KTD5): `deliver_with_guards`
  (drop), peer send, handoff guided injection, feed input route; the
  answered-latch/ask-gate semantics unchanged; unconfirmed-submit refusal
  wired through each route's existing error surface (R5).
- **U7 — attach UX** (KTD6): `leader t` attach chord + dashboard verb;
  `config.terminal` knob; attached badge on the mirror; suppression tuple
  fed by attach hooks (R9).
- **U8 — lifecycle inversion** (KTD7): startup reattach (store ×
  `has-session`, U2 handshake), exited-pane surface for dead marked
  sessions, quit=detach + kill-all confirm variant, clean-exit marker
  re-semantics, cold-boot decision tree (R10: store record + no server ⇒
  today's resume offer).
- **U9 — retirements + flag removal** (KTD9/KTD10): scrollback files, tail
  ring, vte replay, portable-pty path, app-wide delivery mutex; coalescer
  rescoped; watermark constants re-derived for one stream.
- **U10 — automations & monitors on the substrate**: `--paned` dispatch
  spawns a marked session in the automations workspace; monitor
  registration (`resolve_target_now`) and the R22 recursion gate
  unchanged; headless path untouched.
- **U11 — docs + live checklist**: CLAUDE.md architecture rewrite,
  `hooks/CLAUDE.md` KTD11 note, and a live-validation checklist mirroring
  R1–R10 (attention from tmux, restart reattach, attach suppression, feed
  parity, R2 measurement).

## Blast radius (Stage-3 walk of the module map)

| module | fate | note |
|---|---|---|
| `pty/` | **replaced** | `PtyManager` surface (write/resize/close/token/leaf_key/activity/`panes_status`…) re-implemented over U1; read thread, backpressure, tail ring go with U9 |
| `stream/` | **adapted** | `spawn_pane` → U3; coalesce survives for the one piped stream; `set_visible_panes` now also tunes snapshot cadence |
| `state/` (lifecycle, attention, activity, policy) | **untouched** | pure machines; new inputs (pane-died, attach state) map onto existing transitions; policy gains the attached-elsewhere focus input (R9) |
| `hooks/` | **adapted** | pane-token wire/auth unchanged; socket path stabilized (U2); one new token class + op family (KTD12); `CLAUDE.md` note (KTD11) |
| `cli/` | **adapted** | new `substrate/*` event-report ops (KTD12 server-scope token); `fly agents`/`send` unchanged |
| `feed/` | **adapted internally** | wire contracts frozen (R5); screen fallback → capture-pane (KTD9); resolver's ring/vte legs retire; ask registry untouched |
| `peer/` | **adapted** | delivery via U6 ladder; gate order unchanged |
| `automations/` | **mostly untouched** | headless path untouched; `--paned` + monitor registration via U10 |
| `session/` (resume, transcript, handoff) | **simplified** | reattach-first (U8); resume store keeps cold-boot role (R10); transcript logic unchanged; handoff spawns substrate sessions |
| `usage/`, `notify/`, `config/`, `cwd/` | **untouched** | config gains `terminal`, `substrate` (temp), `historyLimit` knobs |
| `lifecycle.rs` | **inverted** | detach-not-reap (KTD7); kill-all variant retains ordered teardown |
| `App.svelte` | **adapted** | focus-tier swap, attach state, spawn/restore paths; polling loop unchanged in shape |
| `Terminal.svelte` / renderer | **narrowed** | focused pane only; T4 WebGL logic retained for it |
| new `Mirror.svelte` | **added** | U5 snapshot renderer |
| `layout.ts`, `workspaces.ts`, `pane-maps.ts`, `keymap.ts`, `home.ts`, `feed.ts`, palette/sidebar/overlays | **untouched** | keymap +1 chord; never-unmount invariant now also guards mirror/terminal tier swaps |
| `write-chain.ts` | **adapted** | order-pinning retained; flush lands as `send-keys -l` |
| `resume.ts`, `handoff.ts` | **simplified** | flag hygiene stays (cold boot); quick/guided handoff delivery via U6 |

## Build log amendments (overnight implementation, 2026-08-11/12)

- **Live-pinned scar (U4): pane death does not end its pipe.** The
  `pipe-pane` `cat` survives the pane process, and `pipe-pane` refuses a
  dead pane ("target pane has exited") — so FIFO EOF can never carry the
  exit. The FIFO reader is a `poll(2)` loop (O_RDWR open, 500 ms tick)
  checking a `forced_exit` slot; the `panes_status` backstop (one
  `list-panes -a`/tick) and the KTD12 hook both feed it. U5's focus-tier
  pipe re-arming must use the same slot discipline, not EOF.
- **Empty-server probe wording (U1): `has-session` against an alive, empty
  server answers "no current target"** — Gas City's `ErrNoCurrentTarget`,
  hit live exactly as their comment predicted; classified as a healthy
  negative, pinned by unit test. Also: a bare `start-server` exits before
  a follow-up `exit-empty off` can land — the options are `;`-chained into
  the starting invocation.
- **KTD8/KTD12 residual for U8 (reattach):** a tmux server that outlives
  fly holds the OLD instance's `FLY_SUBSTRATE_TOKEN`/`FLY_SOCKET_PATH` in
  its env, and armed hooks carry the old fly binary path. Reattach must
  refresh the server env (`set-environment -g`) **and re-arm every marked
  session's hooks**, then live-verify that `run-shell` children observe
  the refreshed values (unverified tmux behavior — test before relying).
- **tpgid (U4):** `/proc/<pane_pid>/stat` field 8 (`tpgid`) is the pane's
  foreground job — full `/proc` parity for cwd/agent-detection/task-count
  with zero tmux round trips; `#{pane_current_path}` not needed.
- **KTD2/U5 revised at build time (better source found):** mirrors render
  from the pane's own hidden xterm buffer, not `capture-pane` — every pane
  keeps ingesting into its mounted, display:none'd xterm (parse-only;
  renderer paused by IntersectionObserver; WebGL gated on
  `visible && !mirrored`), and a 2 Hz `<pre>` snapshot
  (`lib/mirror.ts`) replaces the live render for visible-unfocused panes.
  Substrate-agnostic (relieves the PTY path immediately), zero new IPC,
  no staleness, and **the focus-swap splice protocol is deleted** — the
  buffer is always current, reveal is a display toggle. Consequences:
  pipe-narrowing-to-focused becomes a deferred optimization; the U5 R2
  measurement + a visual check ride the next dev-flavor run; knob
  `mirrorUnfocused` (default on). D8's capture-pane screen-fallback idea
  is likewise superseded — the tail ring/vte can stay until U9 decides.

- **KTD5 amended at build time (U6): unconfirmed submit is retried
  in-place and logged, never surfaced as a refusal.** A refusal invites
  the caller to re-deliver and double-paste; Gas City preserved the same
  handed-to-tmux contract for the same reason. The confirm signal is the
  pane's output-ring seq (substrate-agnostic, no capture-pane): growth =
  the turn started (never re-Enter), static = parked composer (re-Enter,
  ≤3). Revisit surfacing only if the routes ever gain idempotency keys.
  The per-session delivery locks also stay app-wide for now (narrowing
  is an optimization, not correctness).

- **KTD12 token persisted for continuity (U8):** `substrate-server.json`
  (0600, `feed.token` trust class) holds the event token; a new instance
  reloads it, so surviving sessions' armed hooks authenticate with zero
  re-negotiation — sidestepping the unverified question of whether
  `run-shell` children observe post-hoc `set-environment -g`. Proven by the
  cross-instance live test.
- **Adoption replay tradeoff (U8):** capture-then-arm — a ms-scale output
  gap on adopt over visible duplication; ~2k lines replayed, full history
  stays in tmux behind `leader t`.
- **Ephemeral panes (U10):** automation tabs + alerts sink are killed at
  quit (never detached) and their store records pruned — unrestorable
  leaves must not orphan sessions or grow the store. Automation linkage +
  R22 unchanged (same spawn path, link-before-spawn).

## Rollout & validation

1. U1+U2 land inert (wrapper tested pure; socket/token groundwork live —
   itself a small win: hook socket survives restarts).
2. U3–U8 behind `substrate: "tmux"` on the dev flavor; the live checklist
   (U11) runs there, incl. R2's measurement and the S3/S4 protocols.
3. Flag default flips; a full release soak on this box.
4. U9 removes the flag and the portable-pty path; CLAUDE.md rewrite lands
   with it.

Deferred: multi-client attach policy beyond the badge (read-only attach),
control-mode streaming (only if the piped focused pane ever proves
insufficient), non-Claude per-provider submit tables (Gas City's shape,
adopt when a second provider matters), mosh/remote attach.
