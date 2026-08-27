# The fly Electron shell

The **shipped desktop product** since the 2026-08-12 cutover (Electron-shell
migration plan `docs/plans/2026-08-12-002-proposal-electron-shell-migration-plan.md`).
Three processes: this thin Electron main (window, single-instance,
`fly core` spawn-or-adopt, quit orchestration), the headless Rust backend
(`fly core`, the same binary that serves the CLI), and the sandboxed Chromium
renderer running the unchanged Svelte frontend over the preload bridge.

## Files

- `main.js` — window + single-instance, core spawn/adopt + crash
  reconnect (single-flighted through `scheduleRewire`), the control-socket
  bridge, renderer-crash recovery (below), and the ordered quit flow
  (`core/shutdown` → wait → SIGTERM/SIGKILL ladder).
- `recovery.js` — the pure half of renderer-crash recovery (reload budget,
  frame-delivery guard, close plan), tested in `recovery.test.js`.
- `crashed.html` — the page shown when the reload budget is exhausted
  (inert; `R` retries, handled in main).
- `preload.cjs` — the renderer's whole surface (`window.fly`): `invoke`,
  `onEvent`, `onPaneOutput`, `paneInput`, and the quit-confirm pair — plus
  the preload-level close **ack** (`fly:close-ack`, carrying whether the app
  wired a close handler) that keeps main from waiting on a renderer that
  can't answer. The renderer is sandboxed with no node integration; nothing
  else is reachable.
- `protocol.js` — the JS half of the control-socket frame codec.
  **Edited only together with `src-tauri/src/control/` and
  `docs/core-protocol.md`** (the wire contract).
- `no-frontend.html` — inert dev-only page shown when the unpackaged shell
  has neither `FLY_SHELL_URL` nor a built `../dist/` to load. (The U4 bridge
  probe page it replaced is in git history.)
- `build/` — electron-builder's resource dir: `icon.png` (the app icon) plus
  the one-shot toolchain that produced it (`gen-icon.mjs` → `icon-source.png`).
  Not part of any build script.
- `deb/postinst.sh` / `deb/postrm.sh` — .deb lifecycle: SUID
  `chrome-sandbox` (fails the install if it can't — a silent skip would ship
  an app that refuses to launch), the `/usr/bin/fly` symlink that keeps the
  CLI/hooks contract across the Tauri→Electron cutover, and its guarded
  removal.
- This package is a member of the root pnpm workspace (2026-08-27-001 U4):
  `pnpm install` at the repo root installs it, and the root `pnpm-lock.yaml`
  is what makes the shipped .deb reproducible. Its only deps are dev-only
  (`electron`, `electron-builder`) and nothing under `node_modules` ships.

## Dev loop

```bash
pnpm dev            # in the repo root: Vite dev server on :1420
cd electron && npm install
FLY_APP_NAME=fly-el FLY_SHELL_URL=http://localhost:1420 \
  ./node_modules/.bin/electron . --no-sandbox
```

- `--no-sandbox` is **dev-only**: the repo checkout has no SUID helper; the
  packaged app runs fully sandboxed.
- Flavor: a repo checkout defaults to `fly-el` (coexists with an installed
  release); the packaged app is flavor `fly`. `FLY_APP_NAME` overrides both
  and drives the core's config/session/socket roots and this shell's
  userData.
- The fly binary: `FLY_CORE_BIN` env override → the bundled resource
  (packaged) → `../src-tauri/target/debug/fly` → `fly` on PATH.
- Adopt-or-spawn: a live control socket at
  `$XDG_RUNTIME_DIR/<flavor>/control.sock` is adopted (tmux-substrate
  sessions and their backend survive shell restarts); a dead one is
  reclaimed by spawning our own core.

## Renderer crash recovery

The 2026-08-22 incident: under memory pressure (swap full) the Chromium
**renderer** died and the shell sat on a blank window for over an hour while
`fly core` and nine agents kept working; the window could not even be closed.
Now:

- **Reload, re-attach.** `render-process-gone` reloads the frontend (same
  load path as first launch). The core is untouched; on mount every restored
  leaf first asks `adopt_live_pane` and re-attaches to the pane the core
  still owns — same pane id, token, attention registration — sizing its
  xterm to the pane's grid and painting the pane's 64 KiB output tail. Only
  a leaf nobody owns
  spawns. This holds on **both** substrates (a naive reload would orphan
  every pty-backed agent behind a fresh shell).
- **Bounded.** At most 3 reloads per 60 s (`ReloadBudget`); after that the
  window shows `crashed.html` — press `R` to retry, or close the window to
  quit with the ordered core shutdown (tmux sessions survive; the next launch
  re-adopts them).
- **No frame flood.** Control-socket events and pane output go through one
  guarded `sendToRenderer` (frame crashed/destroyed ⇒ drop, one log line per
  outage) instead of 2,000+ "Render frame was disposed" throws.
- **Close always works.** A crashed, hung, never-loaded or crash-page
  renderer gets no say — the close proceeds. A live renderer must ack the
  close request within 3 s (preload-level, so a live event loop always acks);
  no ack ⇒ closed anyway; ack without an app handler ⇒ closed at once; ack
  with a handler ⇒ the busy-agents confirm runs on the user's time as before.
- What is lost: a few ms of pane output in the capture-then-subscribe gap
  (never duplicated), the last ≤ 800 ms of unsaved layout changes, and any
  in-renderer-only state (scroll position, an open overlay). `unresponsive`
  is logged, not reloaded — a renderer mid-way through a huge write recovers
  on its own, and its state is the user's.

The trigger was renderer OOM; memory-limit tuning of the renderer is not
done here. If the crash page keeps coming back, check free memory and swap.

## Logs

The shell and an inherited-stdio core both write to the shell's
stdout/stderr with a `[shell]` prefix — a terminal launch shows them
directly; a desktop launch lands them in the session journal
(`journalctl --user`). There is no log file.

## Packaging

```bash
pnpm build:deb   # repo root: vite build → cargo build --release → electron-builder
                 # → electron/dist-el/fly-electron-shell_<ver>_amd64.deb
```

(`pnpm build` / `pnpm build:core` / `pnpm --filter fly-electron-shell dist`
are the three steps; the dist step refuses to run without a built `../dist`.)
The deb installs as **package `fly`**; the version must be higher than the
installed one for apt to treat it as an upgrade. `version` here, the root
`package.json`, and `src-tauri/Cargo.toml` must agree —
`src/version-lockstep.test.ts` fails otherwise — since the bundled Rust
binary's `fly --version` (and `core/ping`) reports the crate version while
dpkg reports this file's.
