---
date: 2026-08-11
topic: tmux-session-substrate
---

# tmux as the Session Substrate — Requirements

## Summary

Move pane sessions out of fly-owned PTYs into a fly-managed tmux server. fly
stays the orchestrator and projection (attention, dashboard, feed,
automations, triage); the *typing surface* becomes optional — a chord attaches
the focused session in a real terminal, where keystroke latency is native by
construction. The fly window keeps showing every pane, but as cheap mirrors
rather than full xterm.js emulations, ending the webview main-thread
saturation that typing currently queues behind.

This is Path A of the 2026-08-11 direction decision. The evidence that forced
it: `docs/notes/2026-08-11-webkitgtk-engine-floor.md` — after T1/T2/T4 all
landed, the webview main thread still grinds at 60–90% of a core under a few
streaming panes, identically across NVIDIA / no-DMABUF / Intel GL, with WebGL
active. The per-flush cost is WebKitGTK main-thread work upstream of pixels;
no setting recovers it, and keydown shares that thread. The field survey found
no project shipping N interactive xterm.js panes in WebKitGTK; the
terminal-native camp (claude-squad, iTerm2's `tmux -CC`) has native typing by
construction.

---

## Problem Frame

fly's differentiation is the orchestration layer — the hook-fed attention
pipeline, feed, automations, handoff. The terminal emulator was always a
means. Today it is also the bottleneck: the one WebKitGTK main thread renders
every visible pane's VT stream *and* services every keystroke, and at
multi-agent spinner cadence it saturates. The user's comparison point is
"typing into a regular claude terminal," and only a real terminal in the path
closes that gap to zero.

tmux inverts the ownership: the tmux server owns the PTYs and their
lifetimes; fly creates sessions, injects its env, observes output, and
projects state. Any real terminal can attach to the exact same session fly is
mirroring. iTerm2 has run this architecture (GUI as projection over a tmux
substrate) for a decade; claude-squad runs it for exactly our workload.

## What stays invariant (the plan must prove each)

- **The attention pipeline end-to-end.** Hooks are env-injection
  (`FLY_PANE_TOKEN`, `FLY_SOCKET_PATH`) + a Unix socket; tmux passes env
  through session creation. `fly notify` from inside a tmux pane must auth
  and raise exactly as today. The socket security boundary
  (`src-tauri/src/hooks/`) is untouched.
- **Feed, automations, peer messaging, drop** — behavior-identical to their
  wire contracts. Delivery paths that write PTY bytes today
  (`deliver_with_guards`, peer send, drop paste) route through the substrate
  interface instead; same guards, same ordering (guards → publish → paste →
  re-probe → Enter).
- **Frontend never-unmount invariant** — mirrors render flat and keyed;
  split/resize/switch never destroys session state (now even more true: the
  session isn't even in the process).
- **Dev-flavor isolation** — the dev flavor gets its own tmux server
  (`tmux -L` keyed off `FLY_APP_NAME`), mirroring the existing three path
  roots.
- **fly only reads under `~/.claude`** — unchanged.

## What deliberately breaks (product decisions, not casualties)

- **Panes outlive fly.** The tmux server holds sessions across fly restarts
  and crashes. This *inverts* `lifecycle.rs`'s reap-everything shutdown and
  collapses most of the resume problem (reattach, don't respawn — `claude`
  keeps running while fly is closed). Needs explicit semantics: "close pane"
  kills the session; "quit fly" detaches and leaves agents running (with a
  destructive-confirm variant for "quit and kill all"). Cold-boot resume
  (after reboot, when the tmux server is gone) keeps today's `--resume` flow.
- **The typing surface splits.** Deep interactive work happens in an attached
  native terminal; the fly window is overview + triage + light interaction.
  The dashboard's jump-to-agent gains a second verb: mirror in-window vs
  attach natively.
- **Keystroke-perfect in-window emulation for every pane.** Unfocused mirrors
  degrade to snapshot rendering; only the focused pane (at most) gets a live
  VT stream.

## Decision inventory (the plan's future KTDs)

D1. **Observation mechanism**: control mode (`tmux -CC`, `%output` events —
    streaming, exact bytes, one connection) vs plain server + `pipe-pane`
    (raw stream per pane) vs `capture-pane -e` polling (rendered grid, zero
    webview VT parsing). Likely hybrid: control mode for events/lifecycle,
    capture-pane snapshots for unfocused mirrors, streamed bytes only for the
    focused pane. **Spike S2 decides with measurements.**

D2. **Mirror rendering tiers**: does the unfocused mirror keep xterm.js at
    all, or render capture-pane grids as plain styled DOM? Snapshot cadence
    (250 ms–1 s)? Does the focused in-window pane stay a full xterm.js
    terminal (today's path, tolerable when it's the *only* live one)?

D3. **tmux required vs dual substrate.** Prior: required (drop portable-pty
    once parity lands; dual substrate is twice the surface and the current
    path is the one being escaped). Absence of tmux at startup → refuse with
    install hint vs degraded legacy mode.

D4. **Identity mapping**: layout leaf key ↔ tmux session name (both stable).
    Naming scheme, collision policy, whether fly ever adopts sessions it
    didn't create (prior: never — unmarked sessions are invisible to fly).

D5. **Input paths**: in-window keystrokes via control-mode stdin vs
    `send-keys`; programmatic delivery (peer/drop/handoff injection) via
    `paste-buffer`/`send-keys -l` — must preserve bracketed-paste semantics
    and the ESC-cancels-picker hazard analysis. Latency of send-keys measured
    in S3.

D6. **Focus/suppression model**: an externally-attached session counts as
    "focused elsewhere" for `state/policy.rs` suppression (a raise on a pane
    the user is literally typing into must not notify). `client-attached` /
    `client-detached` tmux hooks feed the replicated focus tuple.

D7. **Lifecycle semantics**: startup discovery/reattach of fly-marked
    sessions; orphan sweep (sessions whose automation/purpose is gone); what
    the clean-exit marker means when exit no longer kills panes.

D8. **Scrollback ownership**: tmux `history-limit` becomes the store;
    per-leaf scrollback files retire (mirror attach replays from tmux). What
    happens to the 64 KiB screen-fallback tail ring (`pty/pane.rs`) — likely
    replaced by capture-pane against the live grid, which is *better*
    (`feed/screen.rs` currently re-derives a grid fly no longer has to
    maintain).
- D9. **Backpressure**: tmux buffers pane output; does the coalescer survive
    (for the focused streamed pane only) or retire? Watermark logic likely
    shrinks to one pane's worth.

D10. **Token/env rotation**: per-pane CSPRNG token injected at
    `new-session -e`; what re-mint means when a session outlives a fly
    process (socket path changes per app instance — the reattach handshake
    must re-register live sessions with the new server instance's
    `AttentionManager`; token continuity vs re-mint decided here).

D11. **Attach UX**: terminal launcher (config knob, default
    `x-terminal-emulator`/gnome-terminal), chord placement in `BINDINGS`,
    multiple simultaneous attaches, read-only attach, and the return trip
    (detach hint, fly window re-raise).

D12. **Security note**: same-uid processes can `tmux attach`/`send-keys` into
    any session — formally a wider local input surface than today's PTY fds.
    Threat model unchanged in substance (same-uid could already read config,
    borrow tokens — see dev-flavor techniques note), but the hooks
    `CLAUDE.md` must say so explicitly.

D13. **Subsystem fates** (the Stage 3 walk classifies every module):
    expected *simplified*: `session/resume.rs` (reattach replaces respawn),
    crash auto-offer, scrollback files, screen-fallback ring; *adapted*:
    `pty/` → substrate trait, `stream/` (mirror channels), automations paned
    path, handoff splits; *untouched*: `hooks/`, `feed/` wire, `state/`
    machines, dashboards, keymap (minus new chords).

## Open product questions (owner call, before or during the plan)

- Is in-window typing still first-class (full xterm.js focused pane) or
  explicitly second-class (fine for quick replies, attach for real work)?
- Default jump-to-agent verb from the dashboard: in-window mirror or native
  attach?
- Quit semantics default: leave agents running (recommended) or prompt?

## Spikes (Stage 1 — each answers plan-blocking questions)

- **S1 — hook/env compat** (→ D10, invariants): tmux session with injected
  fly env; claude inside; verify notify auth, SessionStart capture,
  PermissionRequest held-ask over the socket. Include the
  `CLAUDE_CODE_CHILD_SESSION` strip behavior through tmux.
- **S2 — mirror bake-off** (→ D1, D2): measure webview main-thread cost of
  streamed-xterm vs capture-pane-snapshot mirrors under the 5-pane spinner
  protocol (`docs/notes/2026-08-11-webkitgtk-engine-floor.md` method).
  Acceptance target for the architecture: **< 20% average** with 5 streaming
  mirrors (vs 63% today).
- **S3 — input fidelity + latency** (→ D5): send-keys/paste-buffer vs
  today's `pty_write` for typing and for the delivery routes' exact byte
  sequences; bracketed paste; picker-ESC hazard.
- **S4 — lifecycle round-trip** (→ D6, D7, D10): fly restart + reattach to
  live sessions; client-attached/detached hooks driving suppression; native
  attach round trip in gnome-terminal.

## Success criteria for the whole path

1. Typing in an attached terminal is native — nothing of fly in the keystroke
   path (structural; nothing to measure).
2. fly window under the 5-streaming-pane load: webview main thread < 20%
   average, no >50% sustained stretches (measured by the existing protocol).
3. Attention/feed/automations live-validation checklist passes unchanged
   from inside tmux panes.
4. fly restart with 5 live agents: all five reattached, zero respawns, zero
   lost transcript state.
