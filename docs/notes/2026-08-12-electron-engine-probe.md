# Electron engine probe — results — 2026-08-12

Executes `docs/plans/2026-08-12-001-spike-electron-engine-probe-plan.md`. Rig in
`spikes/electron-probe/` (Electron 37 / Chromium, `@xterm/xterm` 5.5 +
WebGL addon fontSize 15 — fly's exact terminal stack — `node-pty`, command
socket injecting at `pty.write`). Same box as every prior number (hybrid-GPU
laptop, Wayland GNOME 46); probe = python-xlib pixel-diff
on the input-line region, ≥12 trials; renderer main thread sampled per-tid at
100 ms. Same-day fly baselines (installed release, tmux substrate, Claude REPL
pane, memory `substrate-typing-latency-numbers`).

## Numbers

**Echo (keypress→pixels, Claude Code REPL pane, p50 over 12):**

| condition | fly (WebKitGTK) | Electron probe |
|---|---|---|
| focused | **98.6 ms** (84–127) | **53.2 ms** (43–83) |
| unfocused | **130.5 ms** (116–170) | **56.5 ms** (37–76) |
| focused, 4 panes flooding | *(not measured)* | **70.1 ms** (53–86) |

**5-pane flood (33 Hz `\r`-spinner in each, window frontmost, 10 s):**

| metric | WebKitGTK (2026-08-11 note) | Electron renderer |
|---|---|---|
| main-thread avg | 63.2 % | **11.7 %** |
| main-thread peak | 80 % | **19.9 %** |
| samples > 50 % busy | 87 % | **0 %** |

Diagnostics: a bare **bash** pane echoes at the same ~70 ms as the Claude pane
under the same load — Ink redraw is *not* the floor; ~50 ms is fixed pipeline
(xterm.js write batching + two-ish 60 Hz frames + compositor). The probe's own
grab+diff polling adds ~10 ms detection overhead **to every row in every
table** (fly's included), so absolute values are overestimates; deltas hold.
Electron's unfocused throttle is mild (57 vs 53 ms) where WebKitGTK's is heavy
(130 vs 99 ms).

## Gate verdict (pre-committed in the plan)

1. Echo p50 ≤ 40 ms: **MISS, narrowly** — 53 ms raw, ~43 ms net of probe
   overhead. Halves fly's 99 ms but does not reach native-terminal feel.
2. Flood main-thread avg ≤ 20 %: **HIT** — 11.7 % vs 63 %, no sample over
   50 %. The scaling wall is simply gone.
3. Under-load degradation < 2×: **HIT** — 1.32× (53 → 70 ms).

## Interpretation

Chromium eliminates the class problem (the flood/main-thread wall that forced
the tmux substrate) and roughly halves typing echo, but xterm.js-in-a-browser
carries a ~40–50 ms pipeline floor on any engine — the probe's bash-pane
result pins that on frame/batching cadence, not on WebKitGTK-specific cost and
not on Ink. So:

- **Option B (Electron swap) is validated for what it actually fixes**: pane
  scaling, background-stream cost, and a ~2× echo improvement. It does not
  buy native typing feel; only camp 2's attach path (or a native surface,
  option C) does that.
- The decision is therefore not "B vs nothing" but **how much the residual
  ~50 ms matters once the substrate's `leader t` native-attach exists for
  typing-heavy work**. B's real payoff is day-to-day: many live panes without
  the WebKitGTK main-thread ceiling and a snappier (if not native) in-window
  echo.
- Known gaps: fly-side typing-under-flood baseline not captured (would need
  a 5-visible-pane layout in the live instance); Wayland-native
  (`--ozone-platform=wayland`) round not run (probe is X11-bound — KTD4's
  optional leg); Electron round used `--no-sandbox` (no SUID helper on this
  box) — irrelevant to render cost.

## Addendum — U6 gate on the real app (2026-08-12, later the same day)

Same method, but the *real* fly frontend on the Electron shell (fly-el
flavor, tmux substrate, Vite dev frontend, `mirrorUnfocused` on as shipped):
echo p50 focused **47.3 ms** (36–50, n=12); 5-pane flood renderer main
thread **13.8 % avg / 39.8 % peak / 0 samples >50 %**; typing with 4 panes
flooding **47.0 ms p50 (1.0×)** — probe region pinned to the focused pane
so flood repaints can't false-trigger the diff. All three pre-committed
gates hit. The real app beats the spike's echo (47 vs 53 ms) — the mirror
throttle keeps unfocused panes off the render path entirely. Renderer
process identification under `--no-sandbox`: forked renderers keep the
zygote's cmdline, so find the process owning a `Compositor` thread.
