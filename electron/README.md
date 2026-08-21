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
  bridge, and the ordered quit flow (`core/shutdown` → wait → SIGTERM/SIGKILL
  ladder).
- `preload.cjs` — the renderer's whole surface (`window.fly`): `invoke`,
  `onEvent`, `onPaneOutput`, `paneInput`, and the quit-confirm pair. The
  renderer is sandboxed with no node integration; nothing else is reachable.
- `protocol.js` — the JS half of the control-socket frame codec.
  **Edited only together with `src-tauri/src/control/` and
  `docs/core-protocol.md`** (the wire contract).
- `probe.html` — the bare-bones U4 bridge probe page, the fallback entrypoint
  when neither `FLY_SHELL_URL` nor a packaged frontend exists. Not a product
  surface; the real dev loop points `FLY_SHELL_URL` at Vite.
- `deb/postinst.sh` / `deb/postrm.sh` — .deb lifecycle: SUID
  `chrome-sandbox` (fails the install if it can't — a silent skip would ship
  an app that refuses to launch), the `/usr/bin/fly` symlink that keeps the
  CLI/hooks contract across the Tauri→Electron cutover, and its guarded
  removal.
- `package-lock.json` — **committed**; this package builds with npm (it is
  deliberately outside the pnpm workspace so electron-builder sees a plain
  node_modules), and the lockfile is what makes the shipped .deb reproducible.

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

## Logs

The shell and an inherited-stdio core both write to the shell's
stdout/stderr with a `[shell]` prefix — a terminal launch shows them
directly; a desktop launch lands them in the session journal
(`journalctl --user`). There is no log file.

## Packaging

```bash
pnpm build   # repo root — the dist/ the shell packages; the dist script refuses to run without it
cargo build --release --offline --manifest-path src-tauri/Cargo.toml
cd electron && npm run dist   # → dist-el/fly-electron-shell_<ver>_amd64.deb
```

The deb installs as **package `fly`** (deliberately colliding with the Tauri
deb's name so installing one replaces the other; version must be higher for
apt to treat it as an upgrade — rolling back to the Tauri deb is a manual
downgrade). Keep `version` here in lockstep with the root `package.json`,
`src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json` — the bundled Rust
binary's `fly --version` (and `core/ping`) reports the crate version.
