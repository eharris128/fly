---
title: "refactor: Electron-only — retire the Tauri shell, unify the build"
type: refactor
date: 2026-08-27
status: executed 2026-08-27 — U0–U7 all landed on main the same day (see the
  results block at the bottom). Answers open decision 3 of
  docs/plans/2026-08-12-002-proposal-electron-shell-migration-plan.md
  ("Tauri retirement horizon") with: retired now, before the repo goes public.
  Rollback = git tag `tauri-shell-final` (proven to build the Tauri deb).
origin: docs/plans/2026-08-12-002-proposal-electron-shell-migration-plan.md
  (KTD9 "stays buildable during transition, retired only after the Electron
  build has soaked as the daily driver" — it has: cutover 2026-08-12, daily
  driver since, renderer-crash recovery landed 2026-08-22)
evidence:
  - the 2026-08-27 pre-open-source review that mapped every Tauri touchpoint
    (Rust, frontend/build, docs) — findings inlined below with file:line
---

# refactor: Electron-only — retire the Tauri shell, unify the build

## Summary

Delete the Tauri desktop shell and everything that exists only to keep it
buildable, leaving **one product**: the Electron shell in `electron/` driving
the headless `fly core` Rust backend over the control socket. Along the way,
fix the build papercuts the dual-shell era left behind — two package
managers, a crate directory named after a framework it no longer uses, a
four-way version lockstep enforced by prose — and drop three repo-root
artifacts that were personal or historical rather than product
(`spikes/`, `skills/`, `packaging/`).

Why now: the repo is about to go public, and the single largest source of
newcomer confusion is that everything is *named* Tauri (`src-tauri/`, `pnpm
tauri dev`, `tauri.conf.json`, 46 `#[tauri::command]` annotations, the
"three places" rule) while the product is Electron. KTD9's soak condition is
met. The macOS best-effort path is the one real casualty (see R9).

The map is favorable: Tauri imports exist in only **four** Rust files
(`lib.rs`, `lifecycle.rs`, `stream/mod.rs`, `notify/mod.rs`), `backend.rs`
and `control/` are already Tauri-free, and `tests/backend_build.rs` already
*is* the post-Tauri boot contract. This is deletion with a few seams to
re-plumb, not a rewrite.

## Requirements

- **R1** After this plan, `grep -ri tauri` over tracked, non-historical files
  (everything except `docs/plans/*`, `docs/brainstorms/*`, `docs/notes/*`,
  `docs/residual-review-findings/*`, and this plan) returns nothing.
- **R2** The complete command surface stays reachable from the packaged app:
  every command the frontend invokes has exactly one Rust body, served by
  `control/registry.rs`. The "three places" rule becomes a two-places rule
  (frontend wrapper + registry). `tests/control_registry.rs` still passes
  unchanged.
- **R3** `cargo test --offline` (all files, incl. `backend_build.rs`),
  `pnpm check`, and `pnpm test:unit` are green at the end of **every** unit,
  not just the last.
- **R4** The packaged deb (`Package: fly`) installs over the current 0.2.0,
  and the live parity list in U7 passes on the installed build: panes,
  attention ring + OS banner, resume offer, quit/relaunch, renderer-crash
  re-attach, `fly notify`/hooks from inside a pane, feed roster.
- **R5** One package manager. `pnpm install` at the root installs everything
  incl. `electron/`; `electron/package-lock.json` is gone; one root script
  builds the deb end to end.
- **R6** Version appears in three files (`package.json`,
  `electron/package.json`, the crate `Cargo.toml`) and a test fails when
  they disagree. `fly --version` prints the crate version.
- **R7** Bare `fly` (and `fly resume`) in a terminal still launches the app
  when the deb is installed; outside the packaged layout it prints help and
  exits non-zero instead of a blank window.
- **R8** Rollback to the Tauri shell remains *possible* from git — a tag
  marks the last commit where it builds — but is no longer a build target.
- **R9** macOS: honestly unsupported. `docs/macos-build.md` is deleted and
  the README says so in one line. (The Tauri mac build was already
  best-effort and `/proc`-degraded; an Electron mac target is a separate
  plan if ever wanted.)
- **R10** The personal skill (`skills/fly-monitor-handoff/`) leaves the
  repo; the verdict-block contract it quoted keeps exactly one home,
  `automations/verdict.rs::VERDICT_BLOCK_SPEC`, and every "edited only
  together" comment that named the skill is corrected.
- **R11** `docs/plans/README.md` and the migration plan's open-decision list
  record the retirement; historical plans are **not** rewritten (their Tauri
  prose is the as-built record — the index already says plan text can lag).

## Key Technical Decisions

- **KTD1 — Delete, don't abstract.** No "shell trait" survives. The
  `BackendSeams` (`backend.rs:37-39`) stay because `fly core` and the tests
  inject different `events`/`banner` closures, but every Tauri arm behind
  them is removed rather than kept behind a feature flag. A feature flag is
  how we got two shells.
- **KTD2 — Tag before deleting.** `git tag tauri-shell-final` on the last
  commit where `pnpm tauri build --bundles deb` works, before U1 lands.
  That is the whole rollback story (R8): checkout the tag, build, install.
- **KTD3 — Registry is the one body.** For each of the 46 commands: if the
  Tauri fn was a thin wrapper over a shared fn, delete the wrapper and keep
  the shared fn; if the registry currently *duplicates* a small body
  (`set_visible_panes`/`set_window_foreground` at `registry.rs:214-238`,
  `pty_write` at `:138-154`, `frontend_log` at `:113-123`), extract that
  body into the owning module as a plain fn and make the registry call it.
  No logic may live only in `registry.rs` — it stays a dispatch table.
- **KTD4 — Banner = `notify-send`, no new crate.** The migration plan's KTD8
  proposed `notify-rust`; the shipped `fly core` has used `notify-send` via
  `spawn_detached_capped` since U7 and got its zombie fix in `7722751`.
  Lift `cli/core.rs:40-61` into `notify/mod.rs` as *the* banner; do not add
  a dependency to replace something that works. The Tauri banner's extra —
  `request_user_attention(Critical)` (`notify/mod.rs:113`) — is a
  **parity check** for the Electron shell (`BrowserWindow.flashFrame` on the
  attention event), verified in U7, not assumed.
- **KTD5 — One lazily-built tokio runtime, owned by the crate.**
  `usage/gate.rs:117` calls `tauri::async_runtime::block_on` from the
  automations sweep thread — a real runtime dependency, not a type. Replace
  with a `OnceLock<tokio::runtime::Runtime>` (current-thread) in `usage/`
  and route `registry.rs:424-430` (which builds a runtime per call today)
  through the same one. `tokio` is already a direct dep (`Cargo.toml:23`).
- **KTD6 — `reqwest` becomes a first-class choice.** It rode into the tree
  "already via tauri" (`Cargo.toml:27`). Keep `default-tls` (system openssl
  is already a runtime dep of the deb) — switching to rustls is out of
  scope; note it as a follow-up in the Cargo comment, nothing more.
- **KTD7 — Bare `fly` execs the shell.** `lib.rs::run` today falls through
  to `tauri::Builder`. After: if argv[1] is a CLI subcommand → CLI (as
  today); else resolve `<dir of this binary>/../fly-shell` (the packaged
  layout: `/opt/fly/resources/fly` → `/opt/fly/fly-shell`) and `exec` it
  with argv passed through, so `fly resume` reaches the shell; if absent,
  print `top_level_help()` plus a one-line "the desktop app is not installed
  beside this binary" and exit 2. `resume` stays out of
  `is_cli_subcommand` (`cli/mod.rs:4-11`, pinned at `lib.rs:796`).
  *To verify in U3:* how `electron/main.js` forwards `resume` to `fly core`
  today (`cli/core.rs:80` resolves launch mode from the core's own argv) —
  if the shell doesn't forward it, add that forwarding in the same unit.
- **KTD8 — `mirrorUnfocused` goes, `renderer: "dom"` stays.** The migration
  plan (`:139-141`) gated the mirror mechanism's removal on exactly this
  retirement; U8 measured it a no-op on Chromium. Remove `lib/mirror.ts`,
  its test, the config key, and the `mirrored` prop plumbing. The sparse
  config store preserves unknown keys, so a user's stale
  `"mirrorUnfocused": true` is silently ignored, not a load error. The
  `renderer` escape hatch is engine-agnostic and cheap; keep it, reword its
  WebKitGTK-motivated comments (`renderer.ts:1-13`).
- **KTD9 — `fly-strip-crossorigin` and `base: "./"` stay.** They read as
  Tauri relics but are load-bearing for Electron's `file://` load
  (`electron/main.js:383`; a `crossorigin` module script under an opaque
  origin fails CORS). Rewrite the comments in `vite.config.ts:5-19` to say
  the `file://` reason and drop the WebKitGTK story.
- **KTD10 — The crate directory is renamed, last.** `src-tauri/` → `core/`
  (matches the role name `fly core`). It's the most-referenced path in the
  repo (CLAUDE.md ×14, electron-builder `extraResources` + `icon`,
  `electron/main.js:39`, `vite.config.ts:28`, `AGENTS.md`,
  `hooks/CLAUDE.md`, `.gitignore`), so it lands as its own unit after the
  code is stable, via `git mv` so blame follows. Historical plans keep the
  old path in prose (R11).
- **KTD11 — pnpm workspace, hoisted if needed.** `electron/README.md:37-39`
  kept `electron/` out of the workspace "so electron-builder sees a plain
  node_modules". Its only deps are `electron` + `electron-builder`
  (devDependencies; `build.files` ships no `node_modules`), so the packed
  app needs nothing from the tree — the concern is only whether
  electron-builder's own tooling tolerates pnpm's symlinked layout. Try the
  plain workspace first; if `dist` breaks, add `node-linker=hoisted` to a
  root `.npmrc` (acceptable repo-wide) rather than reintroduce npm.
- **KTD12 — Vitest picks up `electron/*.test.js` explicitly.** Today it
  happens by default-glob accident (no `test` block in `vite.config.ts`).
  Add `test.include` naming both trees so the contract is written down.
- **KTD13 — The skill is removed, not relocated.** It is install-by-copy
  with no runtime reader and not shipped in the deb; it is Evan's workflow.
  Copy it to `~/.claude/skills/fly-monitor-handoff/` (if not already there)
  in the same step that deletes it from the tree. The monitor feature's
  contract is unaffected: the manager appends `VERDICT_BLOCK_SPEC` to the
  check prompt itself (`automations/mod.rs:1878`); the skill only
  re-quoted it for the *registering* agent.

## Units

Build order. Each unit ends green (R3) and is its own commit(s); U0 can go
in any order and today.

- **U0 — Hygiene deletions** (independent; ~1 h).
  - `git rm -r spikes/` (the measurement record lives in
    `docs/notes/2026-08-12-electron-engine-probe.md` + history). Update
    `README.md:185-186`, `CLAUDE.md:869-871`, `docs/plans/README.md:64`
    (index row keeps its text; the "Primary code" cell notes "removed
    2026-08-27, see git history").
  - `git rm electron/probe.html`; `electron/main.js:380-385` `loadFrontend`
    fallback becomes: `FLY_SHELL_URL` → `loadURL`; else if
    `../dist/index.html` exists → `loadFile` it; else a clear error page
    (reuse `crashed.html`'s styling, "no frontend build found — run `pnpm
    build` or set FLY_SHELL_URL"). Drop the probe prose at `main.js:271,378`,
    `preload.cjs:13`, `electron/README.md:29-31`.
  - Skill (KTD13): copy to `~/.claude/skills/` if absent, `git rm -r
    skills/`. Rewrite `verdict.rs:12-20, 30-36, 135`,
    `cli/automation.rs:831-832` so the const is the sole home; `README.md:65`
    ("taught to agents by the skill" → describe the contract in a clause),
    `README.md:184`, `CLAUDE.md:664`, `docs/plans/README.md:50`. The
    monitor-handoff plan's U8 is historical — add one line under it:
    "skill removed from the tree 2026-08-27 (personal workflow)".
  - `packaging/`: `git mv packaging/gen-icon.mjs packaging/icon-source.png
    electron/build/` (electron-builder's conventional resource dir) with a
    3-line header comment replacing the README; `git rm packaging/README.md`.
    Fix `CLAUDE.md:871-873`. (The icon *move* itself is U5.)

- **U1 — Rust: strip Tauri** (the big one; ~1 day).
  - KTD2 tag first.
  - `Cargo.toml`: drop `tauri`, `tauri-build`, both plugins,
    `[build-dependencies]`; `crate-type = ["rlib"]` (the `staticlib` alone
    was 870 MB of target dir — note in the profile comment); tokio/reqwest
    comments rewritten per KTD5/KTD6. `git rm build.rs`. Remove the
    `windows_subsystem` line in `main.rs:2`. `cargo build --offline` works
    for a dependency *removal*; if the lock needs regenerating, that's the
    one call that needs the sandbox off.
  - `lib.rs`: delete `apply_linux_webview_env`/`nvidia_driver_active`
    (`:32-54`), `raise_alert` AppHandle twin (`:103`), `frontend_log`,
    `register_alert_sink`, `get_launch_mode` (`:93-197`), the whole
    `tauri::Builder` block (`:218-335`). `run()` becomes: CLI dispatch →
    else KTD7 launcher (U3 fills it in; U1 leaves a `help + exit 2` stub).
    `app_dir_name`, `hook_socket_path`, `resolve_launch_mode` stay.
  - `lifecycle.rs`: delete (`Backend::shutdown` at `backend.rs:68` is the
    surviving path, used by `cli/core.rs:181`). `tests/lifecycle.rs` is
    unaffected (it tests `PtyManager::close_all`).
  - `stream/mod.rs`: delete `spawn_pane` (Channel variant, `:133-180`),
    `adopt_live_pane` stub (`:459`), `emit_attention` (`:64`),
    `emit_notification_added` (`:87`, already dead), the `State`-typed
    command fns at `:510-563`; keep `spawn_pane_with`,
    `adopt_live_pane_with`, `attach_pane_now`, `attention_event_payload`,
    `notification_added_payload`, `EventSink`. Extract
    `set_visible_panes`/`set_window_foreground` bodies as plain fns taking
    `&EventSink` (KTD3).
  - `pty/mod.rs:623-800`: drop the 10 command wrappers and both
    `tauri::async_runtime::spawn_blocking` uses; `pty_write`'s
    write-then-attention-clear becomes a plain fn the registry calls.
  - `session/{mod,resume,transcript,handoff}.rs`, `config/mod.rs`,
    `usage/mod.rs`, `feed/mod.rs`, `automations/mod.rs`: strip the
    `#[tauri::command]` attribute and any `tauri::State<…>` parameter; the
    fns are otherwise plain already. Fix the stale invariant comments at
    `resume.rs:380-386, 411-418` ("reachable only from the frontend, never
    the socket" — the registry has exposed both since U2).
  - `notify/mod.rs`: `banner` becomes the notify-send implementation lifted
    from `cli/core.rs:40-61` (KTD4); `cli/core.rs` calls it.
  - `usage/gate.rs:117` + test at `:255-263`: KTD5 runtime.
  - `control/registry.rs`: header (`:1-14`) and the comments at `:43, 421,
    545` rewritten — it is now the *only* command surface, not a twin.
    `stream/coalesce.rs:4` comment: the coalescer's rationale is now purely
    the visible/hidden deadline, not the KTD3 eval quirk — say so.
  - Done when: `cargo test --offline` green, `cargo tree | grep -i tauri`
    empty, `fly core` boots and answers `core/ping`, and the Electron dev
    loop attaches to it.

- **U2 — Frontend: Electron-only transport** (~½ day).
  - `package.json`: remove `@tauri-apps/api`, `@tauri-apps/cli`, scripts
    `tauri`, `build:local`, `build:mac`, `flavor:dev`.
  - `transport.ts`: require `window.fly` (throw a descriptive error at
    first use if absent — a bare browser tab is not a supported target);
    delete the four `@tauri-apps` imports, `OutputSink.channel`,
    `isElectronShell` (unused), the `adoptLivePaneWithSink` null-guard
    (`:214`); make `FlyBridge.onCloseRequested`/`closeNow` non-optional.
    `plainArgs` stays (structured-clone reason). `transport.test.ts`'s
    source-lint suite (`:156-170`) still applies.
  - KTD8 mirror removal: `lib/mirror.ts`, `mirror.test.ts`, `config.ts:88-92`,
    `App.svelte:295-300, 2289, 2602`, the tmux-plan/CLAUDE.md prose.
  - KTD9 comment rewrites in `vite.config.ts`; `clearScreen` comment;
    `renderer.ts:1-13` reword; `App.svelte:1700-1715` worker-ticker comment
    (Chromium also throttles hidden timers — the worker is still right, the
    reason changes). `ipc.ts:533`, `Terminal.svelte:373` drop the Tauri
    caveats.
  - Done when: `pnpm check`, `pnpm test:unit` green; `grep -r "@tauri-apps"
    src vite.config.ts` empty; `pnpm-lock.yaml` no longer lists tauri.

- **U3 — Launcher + version** (~½ day).
  - KTD7 bare-`fly` exec in `lib.rs::run` + a test for the not-installed
    branch (help text, exit 2). Verify/implement `resume` forwarding
    through `electron/main.js` → `fly core` argv.
  - `fly --version` / `-V` in `cli/mod.rs` (add to `is_cli_subcommand`,
    `top_level_help`), printing `CARGO_PKG_VERSION`.
  - R6 lockstep test: `src/version-lockstep.test.ts` reads the three files
    (via `?raw` imports or `node:fs`) and asserts equality. Delete
    `src-tauri/tauri.conf.json` (its `version` was the fourth copy) — this
    is also where `tauri.dev.conf.json`, `capabilities/`, `deb/postinst`
    and the 17 non-`icon.png` icons go (`git rm`).

- **U4 — One package manager** (~½ day, KTD11/KTD12).
  - `pnpm-workspace.yaml`: `packages: ["electron"]`. `git rm
    electron/package-lock.json`; `pnpm install` regenerates the root lock.
  - Root scripts: `shell:dev` (`pnpm --filter fly-electron-shell start`
    with the `FLY_SHELL_URL`/`FLY_APP_NAME=fly-el` env the CLAUDE.md dev
    loop spells out today), `build:core` (`cargo build --release --offline
    --manifest-path …`), `build:deb` (`pnpm build && pnpm build:core &&
    pnpm --filter fly-electron-shell dist`). `electron/package.json`'s
    `dist` keeps the `../dist` guard.
  - `vite.config.ts` `test.include: ["src/**/*.test.ts",
    "electron/**/*.test.js"]`, `exclude` node_modules/dist-el/frontend.
  - Done when: a clean `rm -rf node_modules electron/node_modules && pnpm
    install && pnpm build:deb` produces a deb whose `dpkg -c` listing
    matches the pre-U4 build's (same files, modulo the frontend hash).

- **U5 — Rename `src-tauri/` → `core/`** (~2 h, KTD10).
  - `git mv src-tauri core`; `git mv core/icons/icon.png
    electron/build/icon.png`, `git rm -r core/icons`.
  - Update: `electron/package.json:28-31` (`extraResources.from` →
    `../core/target/release/fly`), `:37` (`icon` → default `build/icon.png`,
    so the key can go), `electron/main.js:39`, `vite.config.ts:28`,
    `.gitignore` (section headers + paths), `AGENTS.md:20,25`,
    `core/src/hooks/CLAUDE.md:131-133`, every `--manifest-path` in
    CLAUDE.md/README/electron/README, `electron/protocol.js:2`,
    `tests/headless_runner.rs:24` / `feed_server.rs:408` / `drop.rs:881`
    (`CARGO_MANIFEST_DIR`-relative — unaffected, but check).
  - Done when: `git grep -n "src-tauri"` hits only `docs/plans`,
    `docs/brainstorms`, `docs/notes` (R1 carve-out) and this plan.

- **U6 — Docs sweep** (~½ day). Each earlier unit fixes the lines it
  invalidates; this is the consistency pass against R1/R11.
  - `README.md`: `:7-12` drop the rollback sentence; `:140-154` delete the
    Tauri subsection; `:164-186` layout — transport bullet, "both shells",
    remove `skills/`/`spikes/` lines; one line: "Linux only; the macOS
    Tauri build was retired 2026-08-27 (`tauri-shell-final` tag)."
  - `CLAUDE.md`: `:22-23` stack line; `:27-53` commands (Electron block
    becomes the only block, with the new root scripts); `:55-57` system
    deps (Electron's runtime deps instead of WebKitGTK); `:59-81` gotchas
    (drop the tauri-dev and crossorigin-blank-window items; keep the
    Wayland-screenshot and cargo-sandbox ones; `frontend_log` note →
    "renderer devtools exist; `frontend_log` still lands uncaught renderer
    errors in the core's stderr"); `:83-117` packaging + side-by-side
    (`FLY_APP_NAME` flavor story stays, the plugin/dev-conf machinery
    goes); `:129-134` "three roles" → two; `:195-199` mirror prose;
    `:206-210` KTD3-eval paragraph; `:386-431` backend/control bullets;
    `:447-455` **two-places rule**; `:711`; `:806-819` transport bullet;
    `:869-879` oddities + versioning (three files).
  - `electron/README.md:37-39` (workspace reason inverted), `:106-119`
    packaging + lockstep; `docs/core-protocol.md:68` ("`cmd` names are
    exactly the Tauri command names" → "…are the names `ipc.ts` sends;
    `control_registry.rs` pins them — renaming is a wire change").
  - `docs/plans/README.md`: index row for this plan; `:65` migration row
    gains "Tauri shell retired 2026-08-27 → 2026-08-27-001"; `:101-102`
    drop the macOS line; `:64` spike row per U0.
  - Migration plan `:298-312`: answer open decision 3 with a pointer here
    (append, don't rewrite). tmux LIVE-CHECKLIST `:1-30` setup: remove the
    Tauri variant lines; check 3 (mirrors) becomes "retired with
    `mirrorUnfocused` — N/A".
  - `git rm docs/macos-build.md` (R9).

- **U7 — Live validation + install** (~2 h; the R4 gate).
  1. `pnpm build:deb`; `sudo apt install ./electron/dist-el/fly_*.deb`
     (this also finally installs the post-`14beea4` renderer-crash
     recovery, which the current install predates).
  2. Launch from the desktop entry; open 3 panes, `claude` in one; trigger
     a permission ask → ring + `notify-send` banner + (KTD4 check) window
     urgency via `flashFrame`.
  3. `fly --version`; bare `fly` from a terminal → window focuses (KTD7);
     `fly resume` after a quit → resume offer appears.
  4. Quit with an agent mid-work → busy confirm; relaunch → session
     restored. Kill the renderer (`kill -SEGV` the renderer pid) →
     `crashed.html` → reload re-attaches (`adopt_live_pane`).
  5. Dev loop: `pnpm dev` + `pnpm shell:dev` beside the installed app
     (`fly-el` flavor isolation intact).
  6. Feed: `curl -H "Authorization: Bearer …" localhost:4939/feed` streams.
  Record results as a dated block at the bottom of this plan.

## Blast radius

| Area | Verdict | Notes |
|---|---|---|
| `backend.rs`, `control/*`, `hooks/`, `feed/`, `peer/`, `state/`, `substrate/`, `automations/` (minus 5 attrs) | untouched | already Tauri-free |
| `stream/`, `pty/`, `notify/`, `lib.rs`, `lifecycle.rs` | rewritten/deleted | the four files with real imports + the wrappers |
| `usage/gate.rs` | one runtime swap | KTD5 |
| Control protocol (`docs/core-protocol.md`) | unchanged on the wire | names pinned by `control_registry.rs`; only prose changes |
| Frontend (`src/`) | `transport.ts` + mirror removal + comments | no behavior change on Electron |
| Electron shell | `probe.html` gone, `loadFrontend` fallback, icon path, `resume` forwarding (maybe) | |
| tmux substrate | untouched | its own open bug (`tmux-substrate-withholds-output`) is a separate workstream |
| macOS | **lost** | R9 |
| Deb contents | identical modulo hashes | U4 done-criterion |

## Risks

- **`resume` reaching the core** (KTD7). If today's shell never forwards
  argv, the "resume offer" path may currently work only via the
  clean-exit-marker heuristic; U3 must read `electron/main.js:149-174`
  before assuming. Mitigation: it's called out as verify-first.
- **electron-builder vs pnpm** (KTD11). Known to have been fragile with
  symlinked `node_modules` in older versions. Mitigation: the hoisted
  fallback; the done-criterion diffs the deb listing.
- **Window urgency parity** (KTD4). Losing the Tauri `Critical` attention
  hint silently would be a regression in the signature feature. Mitigation:
  explicit U7 step 2.
- **Docs drift.** 36 real shell references in CLAUDE.md alone. Mitigation:
  R1 is a grep, run as the last step of U6.
- **KTD-ID collisions.** "KTD9" means *rollback* in the migration plan,
  *tiered detection* in the foundation plan, *gate order* in peer messaging,
  *spawn race* in audit-remediation. When editing CLAUDE.md `:33, :130`,
  resolve against the migration plan only.

## Validation

- R1: the grep, with the carve-out, empty.
- R2/R3: `cargo test --offline`, `pnpm check`, `pnpm test:unit` after every
  unit; `tests/control_registry.rs` and `tests/backend_build.rs` unchanged.
- R4: U7 list, recorded below.
- R5: the clean-checkout `pnpm install && pnpm build:deb` in U4.
- R6: the lockstep test, plus a deliberate mismatch to see it fail once.
- R7: U7 step 3.
- R8: `git checkout tauri-shell-final && pnpm tauri build --bundles deb`
  once, right after tagging, to prove the tag is what it claims.

## Deferred (explicitly not this plan)

- Electron on macOS / Wayland leg of migration U6 (still "box on X11").
- `reqwest` → rustls (KTD6 note).
- The tmux `pipe-pane` output-withholding bug and the substrate checklist.
- A skill installer (`fly hooks setup`-style) — the skill left the repo;
  if it ever comes back it comes back as a shipped, installed artifact.
- Plan status headers across `docs/plans/` (the open-source hygiene
  item that motivated this plan is tracked separately).

---

## Results (2026-08-27, as-built)

Executed in order U0 → U7 in one session; each unit is its own commit on
`main` (`b0b65a8` U0 … `9c0c2ca` U7). Three corrections against the text
above, recorded rather than silently diverged from:

- **KTD7's verify-first item had a real finding.** The shell spawned `fly
  core` with no argv, so `fly resume` had reached nothing since the cutover —
  only the crash-marker *offer* path worked. U3 made the CLI binary exec
  `/opt/fly/fly-shell` with argv passed through, the shell forward `resume`
  to the core it spawns (`fly core resume`), and `resolve_launch_mode` take
  the bool. Live-verified: the packaged shell launched as `fly-shell resume`
  spawns `fly core resume` and `get_launch_mode` answers `"resume"`.
- **KTD4's parity check found the gap.** `main.js` had no window-urgency
  handling at all. U7 added `electron/urgency.js` (pure `shouldFlash`,
  tested) wired into the event bridge, cleared on focus.
- **A second real bug fixed in passing:** a losing second launch raced past
  `app.quit()` into `whenReady`, adopted the core and logged `ERR_FAILED`
  loading the frontend. Now routine (bare `fly` execs the shell), so guarded.

Gates:

- R1: `git grep -i tauri` outside the historical doc dirs returns only the
  deliberate retirement notes. `cargo tree | grep -i tauri` is empty.
- R2/R3: `cargo test --offline` (814 lib + every integration file),
  `pnpm check`, `pnpm test:unit` (417, incl. `electron/*.test.js` and the
  new lockstep test) green at the end of every unit. One integration test
  (`headless_runner::kill_run_seam…`, a 5 s bounded poll) failed once under
  the fat-LTO release build's CPU load and passed on re-run.
- R5: a clean `pnpm install` + `pnpm build:deb` produced a deb whose
  `dpkg -c` file set is **identical** to the pre-U4 build (92 entries).
- R6: `fly --version` → `fly 0.2.0`; `src/version-lockstep.test.ts` in the
  run.
- R7: bare `fly` with no shell → help + exit 2; with `FLY_SHELL_BIN` → exec
  with argv (same pid). The real installed layout matches the derivation
  (`/usr/bin/fly → /opt/fly/resources/fly`, `/opt/fly/fly-shell`).
- R8: `tauri-shell-final` tagged at the post-U0 commit; the Tauri deb built
  from it in a scratch worktree (needed the frontend prebuilt and
  `beforeBuildCommand` blanked — pnpm's deps check refuses a symlinked
  `node_modules`).
- R4 (live, on the packaged artifact extracted from the deb, `fly-el`
  flavor, installed app untouched throughout — its core pid 47783 ran the
  whole time): sockets bound + `core/ping`; feed `/healthz` 200; a raise
  from the spawned pane's own token → `pane://attention raised` on the
  socket; with the window replicated as backgrounded, the same raise →
  D-Bus `Notify` from `notify-send` with the exact title/body (KTD4 banner);
  renderer SIGSEGV → `renderer gone … exitCode=139` → reload → new
  renderer, **same pane pid** (adopted, no `pane://exit`); `core/shutdown`
  → `shutting down (ordered)` → core exit → shell respawned a core; `fly
  resume` from the packaged binary while running → single-instance handoff,
  no second shell. Not exercised without a keyboard: the busy-agents quit
  confirm and the actual `flashFrame` (its pure rule is unit-tested; the
  window was focused throughout).

**Not done, deliberately:** the deb was **not installed** — it would replace
the daily driver mid-session. `electron/dist-el/fly-electron-shell_0.2.0_amd64.deb`
is the artifact (same version as the installed one, so `sudo apt install
--reinstall ./…deb`, or bump the version first). Installing it also finally
ships the renderer-crash recovery the installed build predates.

