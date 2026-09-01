# Spike: Electron engine probe — 2026-08-12-001

**Status**: executed 2026-08-12 — results in
`docs/notes/2026-08-12-electron-engine-probe.md` (gate: 2/3 hit; flood wall
eliminated, echo halved but ~50 ms xterm.js pipeline floor persists on any
engine)
**Type**: spike (throwaway measurement rig, not a feature)
**Follows**: `docs/notes/2026-08-11-webkitgtk-engine-floor.md` (the survey and
the A/B/C fork), `docs/notes/2026-08-08-typing-latency-diagnosis.md`, and the
2026-08-12 live measurements on the installed release + tmux substrate
(~99 ms p50 keypress→pixels focused, ~130 ms unfocused; WebKitWebProcess
~88 %/core at 20 Hz typing; tmux transport < 1 ms — recorded in memory
`substrate-typing-latency-numbers`).

## Question

Option B (Electron engine swap) rests on one empirical claim: **Chromium
erases the ~90 ms WebKitGTK render leg for fly's exact workload on this exact
box** (a hybrid-GPU laptop, Wayland, GNOME 46). VS Code
is the existence proof elsewhere; nothing has measured it *here*. This spike
produces that number and nothing else.

## Non-goals

- No fly integration: no Tauri port, no hook socket, no attention pipeline,
  no session persistence. The spike never touches fly's code or state dirs.
- No packaging, no lint, no tests, no review polish. The rig is disposable;
  the deliverable is the results note and the go/no-go.
- Not a UX prototype — nobody types into this thing for feel; the probes do.

## Key technical decisions

- **KTD1 — engine is the only variable.** The spike pins the same terminal
  stack fly ships: `@xterm/xterm` 5.5.x + `@xterm/addon-webgl` 0.18.x
  (+ fit/unicode11), same font size (fly config default 15), comparable cols
  (~100×60, matching the measured half-width window). Rendering differences
  must be attributable to Chromium vs WebKitGTK, not to versions or config.
- **KTD2 — injection enters at `pty.write`.** Today's fly numbers start at
  `tmux send-keys` (input already past the keyboard/webview leg). The spike's
  control channel calls `node-pty`'s `write()` directly — the equivalent entry
  point — via a line-oriented command socket (Unix socket in the scratchpad;
  `TYPE <hex>` → pty write). Latency numbers are then apples-to-apples.
- **KTD3 — identical probe methodology.** The same python-xlib pixel-diff
  probe (region hash of the input line, blink-calibrated threshold, ≥12
  trials, p50/min/max) and the same 33 Hz spinner-cadence 5-pane flood
  protocol from the engine-floor note. New numbers land in the same tables.
- **KTD4 — stock Electron first, then windowing variants.** Round 1 runs
  Electron with default flags (X11/Xwayland on this setup — also what the
  probe needs, since xwd/XTEST can't reach a pure-Wayland surface). Only
  after stock numbers are recorded, optionally re-run with
  `--ozone-platform=wayland` (probe limited to CPU sampling there — no pixel
  probe) to check the compositing path isn't Xwayland-flattered.
- **KTD5 — spike lives in `spikes/electron-probe/`**, committed (it documents
  the decision), with its `node_modules` gitignored. The npm install needs the
  Bash sandbox off (registry fetch); everything after runs sandboxed.

## Units

- **U1 — the rig.** `spikes/electron-probe/`: `main.js` (one BrowserWindow,
  `nodeIntegration` on — it's a lab bench, not a product), `index.html` +
  `renderer.js` (xterm.js + WebGL addon, dark theme, font size 15),
  `node-pty` spawning `bash` per pane. Layout switchable 1-pane / 5-pane grid
  via CLI arg. The command socket (KTD2) supports `TYPE`, `SPAWN <n>`, and
  `TITLE` (stamps a distinctive window title for the probe to find).
- **U2 — probe parity harness.** Adapt the existing scratchpad probes to
  target the spike window: pixel-latency probe (KTD3) + per-tid renderer
  main-thread CPU sampler (Chromium splits processes; sample the renderer
  process' main thread, identified via `/proc/<pid>/task` comm + start order,
  the analogue of the WebKitWebProcess main-thread numbers).
- **U3 — single-pane echo round, real `claude`.** One pane running an
  interactive `claude` REPL (the Ink input-line redraw is the amplified,
  realistic per-key cost — same condition as the 2026-08-12 fly measurement).
  Record p50/p90/min/max focused and unfocused, ≥12 trials each.
- **U4 — flood round.** 5 panes, 33 Hz `\r`-spinner writes into each pty from
  the harness (bytes enter at the pty like the engine-floor protocol), window
  frontmost, renderer main thread sampled at 100 ms for 10 s. Record
  avg / peak / %-of-samples->50 % against WebKitGTK's 63.2 % / 80 % / 87 %.
- **U5 — typing-under-load round.** U4's flood running in 4 background panes
  while U3's echo probe types into the 5th (focused). This is the actual
  day-to-day complaint (typing while agents stream); fly's number for this
  condition can be captured the same afternoon if missing.
- **U6 — results note + decision.** `docs/notes/2026-08-12-electron-engine-probe.md`:
  the tables, the verdict against the gate below, and the recommendation.
  Update memory (`webkitgtk-engine-floor` gains the Chromium column;
  `substrate-typing-latency-numbers` links the note).

## Decision gate (set before any measurement)

Camp 3 (Electron swap) is **justified** iff, on this box, stock-flags round:

1. Single-pane echo (U3, focused): **p50 ≤ 40 ms** keypress→pixels.
2. Flood (U4): renderer main-thread **avg ≤ 20 %** (target single digits).
3. Typing under load (U5): echo p50 degrades **< 2×** vs U3.

All three hit → draft the option B migration plan (Svelte+xterm frontend
verbatim on Electron; Rust backend as sidecar over the existing socket/feed
seams). Any miss → close camp 3 in the results note; camp 2 (orchestrator +
native attach) stands as the architecture by default, and further effort goes
to attach-UX polish, not engines.

## Risks / notes

- `electron` npm download is ~90 MB and needs the sandbox disabled for the
  install step only (known env constraint; everything else offline).
- `node-pty` needs a compile (node-gyp) — `build-essential` present.
- An occluded/unfocused Chromium window throttles like WebKit does — probes
  must verify focus state (the existing probe already checks and the fly
  numbers showed a 30 ms focus delta; record focus state per round).
- Keep probe keystrokes out of the `claude` REPL's submit path (type + `C-u`,
  never Enter) — same discipline as the fly rounds.
- Budget: roughly half a day including the writeup. If the rig fights the
  harness for longer than that, timebox: record what's solid, note the gaps.
