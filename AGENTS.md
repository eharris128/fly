# fly — agent guide

**fly** is a desktop "terminal for AI coding agents" for Ubuntu/Linux: real
PTY-backed panes, tabs + splits (one agent per pane), and an agent-attention
indicator + OS notification when an agent needs you. v1 integrates **Claude
Code** as the attention source. Stack: **Tauri v2** (Rust) + **Svelte 5** (Vite/
TS) + **xterm.js**. The full design lives in `docs/plans/`.

## Run / build / test

```bash
pnpm install                  # frontend deps (works sandboxed; npm registry allowed)
pnpm tauri dev                # run the app (Vite dev server + cargo run)
pnpm check                    # svelte-check (type-check the frontend)
pnpm test:unit                # vitest — frontend unit tests (layout tree, keymap)
cargo test --offline --manifest-path src-tauri/Cargo.toml   # Rust tests
pnpm tauri build              # release .deb (primary) + AppImage (secondary)
```

### Environment gotchas (this machine)

- **cargo needs the sandbox disabled** for anything that resolves/fetches new
  crates: `index.crates.io` is blocked by the Bash sandbox allowlist (curl to it
  times out), while `static.crates.io` and npm work. Run new-dependency builds
  with `dangerouslyDisableSandbox: true`; once deps are cached,
  `cargo build/test --offline` works sandboxed. See the memory note.
- **System deps** (Tauri/WebKitGTK): `libwebkit2gtk-4.1-dev build-essential
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf`.
- **Dev binary vs release**: a debug `cargo build` binary loads the frontend
  from `devUrl` (the Vite server), so it shows blank if run standalone — use
  `pnpm tauri dev`. The **release** build (`pnpm tauri build`) embeds the
  frontend and runs standalone.
- The webview console is invisible; the frontend forwards uncaught errors to
  stderr via the `frontend_log` command (look for `[fly-webview]`).

## Architecture

Backend (`src-tauri/src/`):

- `pty/` — `PtyManager` registry + `Pane`: portable-pty, one read thread per
  pane, ordered reap-on-exit, backpressure pause/resume (KTD13/KTD4).
- `stream/` — `spawn_pane` + raw-byte output over a Tauri `Channel` (KTD3); the
  pane↔attention/token wiring; focus/foreground commands.
- `state/` — `lifecycle` + `attention` machines (pure, test-first) + `suppress`
  matrix + `manager` (per-pane registry, KTD8).
- `hooks/` — authenticated Unix-socket hook channel: per-pane CSPRNG `token`,
  constant-time compare, `SO_PEERCRED`, lockout (KTD7). Security boundary.
- `cli/` — the `fly` CLI: `notify`, `hooks setup`/`teardown` (KTD12).
- `notify/` — OS notification + sanitization + coalescing/rate-limit gate.
- `config/`, `session/`, `cwd/`, `lifecycle.rs` — settings, persistence, `/proc`
  cwd tracking, graceful shutdown.

Frontend (`src/`): `App.svelte` orchestrates tabs + the split tree;
`lib/layout.ts` is the pure split-tree model (flat, keyed rendering so splits
never respawn a pane); `lib/Terminal.svelte` is an embeddable xterm leaf;
`lib/keymap.ts` is the leader-key model; `lib/{config,serialize}.ts` + `ipc.ts`.

## Using agent-attention

`fly hooks setup` installs a Claude Code `command` hook in
`~/.claude/settings.json` (backed up first) that runs `fly notify`. Inside a fly
pane, Claude Code's `Notification`/`Stop` events then raise the pane's attention
ring + an OS notification. `fly hooks teardown` removes only fly's hooks.

## Conventions

- Match the surrounding style. Rust modules are heavily doc-commented with the
  KTD/requirement they implement. Tests accompany behavior-bearing units.
- Commit messages: conventional, with a `Co-Authored-By: Claude` trailer.
