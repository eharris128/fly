# WebKitGTK engine floor — GPU/driver A/B — 2026-08-11

Follow-up to `2026-08-08-typing-latency-diagnosis.md`. After T1 + T2 + T4 all
landed, typing lag persists in day-to-day use. This session tested the last
in-architecture suspects — the GPU/presentation path — and eliminated them.
Conclusion up front: **the remaining cost is WebKitGTK main-thread work per
flush (JS + VT parse + xterm render orchestration + layout), not the GL leg.
No environment variable or renderer setting recovers it. The current
architecture is at its engine floor.**

## Why the GPU path was suspect

- The dev machine is a hybrid-GPU laptop: the desktop composites on the
  integrated GPU while the webview's GL was live-confirmed (driver mapped in
  WebKitWebProcess, device fds open) on the discrete one, the exact
  WebKitGTK + discrete-GPU DMABUF combination Tauri's Linux-graphics docs
  flag for high input latency, with cross-GPU buffer traffic per frame.
- WebKitGTK masks the WebGL renderer string ("Apple GPU"), and WebGL context
  creation succeeds even on a software rasterizer — T4's `auto` cannot detect
  a slow path. (Software rasterization was ruled out directly: no `llvmpipe-*`
  threads in WebKitWebProcess.)

## Method

Installed release, window frontmost and presented (an occluded window's frame
clock throttles and render cost vanishes — a 5-pane flood measured **6%** CPU
until the window was raised; measurements below are with the window front).
Load: spinner-cadence writes (`\r`-line, 33 Hz) into every pane shell's
**slave pts** from outside — bytes reach fly's PTY reader without keyboard
input. Webview main thread sampled at 100 ms for 10 s
(protocol from the 2026-08-08 note). The claude pane's Ink UI visible in
rounds 1 and 3 (per-flush repaint region far larger than the bare spinner).

| config                                   | GL driver     | avg   | peak | >50% busy |
|------------------------------------------|---------------|-------|------|-----------|
| baseline (DMABUF renderer)               | discrete      | 63.2% | 80%  | 87%       |
| `WEBKIT_DISABLE_DMABUF_RENDERER=1`       | discrete      | 47.0%* | 100% | 53%*     |
| `__EGL_VENDOR_LIBRARY_FILENAMES=…mesa…`  | integrated    | 65.7% | 90%  | 86%       |

\* load generator stalled partway through this round (a pane hit backpressure
→ pause → kernel PTY buffer filled → blocked writer), so the stimulus was
weaker; treat as "modest improvement at best," not a win. The integrated-GPU round ran
its full load with *fewer* panes (3 vs baseline's 5) and still matched
baseline — the strongest evidence that the driver/presentation path is
irrelevant.

`renderer` is not pinned in `~/.config/fly/config.json`, so T4's `auto`
(WebGL on visible panes) was active in every round.

## Interpretation

- ~60–90% of one core of webview **main-thread** time under a few streaming
  panes, invariant across discrete GPU / no-DMABUF / integrated GPU. The cost is upstream of
  presentation: channel `eval` + JS, VT parse, xterm buffer/render
  orchestration, and layout, all sharing the one thread that also services
  keydown. WebGL moved cell *rasterization* off the DOM but the per-flush
  main-thread orchestration cost remains the ceiling.
- Confirms and closes the 2026-08-08 note's trajectory: after three correct
  in-architecture fixes, the floor is the engine itself. Chromium runs the
  identical workload (many xterm.js+WebGL panes) comfortably — VS Code is the
  existence proof; WebKitGTK does not.

## Consequences

Remaining in-architecture mitigations (worth doing, won't change the class):
1. **Focused-pane flush priority** — only the focused pane keeps the ~4 ms
   coalesce deadline; visible-but-unfocused panes drop to ~50–100 ms. Cuts
   main-thread render jobs ~Nx while typing. Cheap, in `stream/coalesce.rs`
   (the visibility-aware deadline already exists; add a focus tier).
2. The un-owned audit #1 (feed resolver full transcript re-read / 1.5 s) and
   T5 (absolute activity stamps) — background CPU competing with input.

The class change is architectural; the 2026-08-11 survey of comparable
projects found no one shipping fly's combination (N interactive xterm.js PTY
panes inside WebKitGTK). The field: headless stream-json chat GUIs
(opcode/Claudia, Conductor, Crystal), tmux-backed native TUIs (claude-squad —
attach = native typing by construction), xterm.js on Chromium (VS Code,
Electron apps), or native GPU grids (Zed via `alacritty_terminal` + GPUI).
Candidate directions, in increasing cost: **(A)** tmux-backed panes with a
native-terminal attach escape hatch (fly stays orchestrator; hooks/socket/feed
unchanged — env passes through tmux); **(B)** Electron engine swap (same
Svelte+xterm.js frontend on Chromium; Rust backend survives as a sidecar);
**(C)** native grid surface (leave the webview for the terminal area
entirely).
