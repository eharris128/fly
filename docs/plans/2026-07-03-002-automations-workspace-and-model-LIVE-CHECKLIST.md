---
title: "TODO: live verification — automations dedicated workspace + model/effort"
date: 2026-07-04
type: checklist
status: pending
plan: docs/plans/2026-07-03-002-feat-automations-workspace-and-model-plan.md
---

# Live verification checklist (follow-up)

The `feat: automations dedicated workspace + per-automation model/effort` work is
**implemented and committed to `main`** (commit `94c92b2`). Every behavior-bearing
unit ships with tests and the full suite is green:

- Rust: `cargo test --offline --manifest-path src-tauri/Cargo.toml` → 288 lib + all
  integration tests pass.
- Frontend: `pnpm check` clean; `pnpm test:unit` → 872 tests pass.

What remains is **live** exercise in the running app. This file is the pick-up-cold
TODO for that.

## Already confirmed live
- **U2 (CLI flags + validation)** — run against the freshly-built
  `src-tauri/target/debug/fly`:
  - `automation create --help` lists `--model` / `--effort`.
  - `--effort bogus` → exit 2 (valid-set message).
  - `--model` combined with `--script` → exit 2 (agent-mode only).

## Prerequisites / gotchas
- **Launch a dev build (isolated state), never a bare debug binary standalone:**
  in a terminal at the repo root, `pnpm flavor:dev` → window titled **fly (dev)**.
  It uses `FLY_APP_NAME=fly-dev`, so config/session/socket are separate from the
  installed app (`~/.local/share/fly-dev/`, `~/.config/fly-dev/`,
  `$XDG_RUNTIME_DIR/fly-dev/`). Do NOT run `pnpm flavor:dev` from an agent
  background task — the harness reaps it (Vite dies, the app orphans without its
  dev server). Run it in a normal terminal you own.
- **Use the dev binary for `fly automation …` inside the dev pane**, i.e.
  `FLY=~/projects/fly/src-tauri/target/debug/fly`. The `fly` on `PATH`
  is the installed `/usr/bin/fly`, which as of this writing is **stale** (no
  `automation` subcommand, no `hook_event`), so it cannot drive the new flags and
  its Stop hook will not close automation runs. Mutating ops (`create`/`run`)
  must run from inside a fly pane (they need `FLY_PANE_TOKEN`/`FLY_SOCKET_PATH`,
  injected by the app).

## Test A — dedicated Automations workspace + model-at-launch + dashboard (U7 / U4a / U9)
No hook changes needed.

1. In the dev window's shell pane:
   ```bash
   FLY=~/projects/fly/src-tauri/target/debug/fly
   $FLY automation create --name livetest --cron '*/5 * * * *' --tz America/New_York \
     --prompt 'Reply with exactly the word DONE, then stop.' --model sonnet --effort low
   # copy the printed id, then fire it immediately (skip the 5-min cron wait):
   $FLY automation run <id>
   ```
2. **Expect:** a new **"Automations"** workspace appears in the sidebar and the run
   opens there as a background tab (not in the current workspace) — R1/R3/U7.
3. **Expect:** the agent tab launches
   `claude --dangerously-skip-permissions --model sonnet --effort low …` — the
   model/effort are applied explicitly (U4a/R11). (`fallback-model` is omitted here
   because the resolved primary `sonnet` equals the default fallback.)
4. **Expect:** `Ctrl-A` then `d` (dashboard) shows the `livetest` row with a chip
   reading **`sonnet · low`** (U9/R13).
5. **Headless cross-check** (any terminal): the stamped RunRow —
   ```bash
   cat ~/.local/share/fly-dev/automations.json | python3 -m json.tool | grep -A2 -iE '"model"|"effort"'
   ```
   should show `model: "sonnet", effort: "low"` on the run row (R13). Also verify a
   run created with **no** `--model` (and no shared default) stamps `null`/omits the
   flags (Claude default).

## Test B — restart durability of the workspace marker (U6 / R2)
1. With the Automations workspace present, fully quit and relaunch `pnpm flavor:dev`.
2. **Expect:** the next automation run lands back in the **same** "Automations"
   workspace (resolved by the persisted `role: "automations"`, not the in-memory id).
   Confirm the marker persisted:
   ```bash
   grep -o '"role":"automations"' ~/.local/share/fly-dev/session.json
   ```

## Test C — auto-close on success + transcript output capture (U8 / U4b / U5)
These fire on the agent's **Stop**, delivered via `~/.claude/settings.json`. That
hook currently points at the stale `/usr/bin/fly` (no `hook_event`), so the dev app
never gets the close and the run falls to the 30-min deadline. To exercise it, the
Stop hook must run a `fly` that sends `hook_event`. **Pick one:**

- **Rebuild + reinstall the stable app** (preferred, permanent):
  `pnpm tauri build --bundles deb && sudo apt install ./src-tauri/target/release/bundle/deb/fly_*_amd64.deb`,
  then re-run `fly hooks setup` so the global hook points at the fresh binary.
- **Temporarily repoint the hook at the dev binary** (reversible):
  ```bash
  cp ~/.claude/settings.json ~/.claude/settings.json.bak
  ~/projects/fly/src-tauri/target/debug/fly hooks setup   # rewrites the fly hooks to the dev binary path
  # … run Test A again, let the agent reply DONE and Stop …
  # restore afterwards:
  cp ~/.claude/settings.json.bak ~/.claude/settings.json
  ```
  Caveat: while repointed, the installed stable app's notifications also route
  through the dev binary (compatible, but restore when done, and note the dev
  binary path must exist).

Then, with a Stop-capable hook:
1. Run an agent automation (Test A). Let the agent produce its final message and Stop.
2. **Expect (U8/R6):** the succeeded run's background tab **auto-closes ~6s** after
   Stop. A run that instead raised a real mid-run question, or failed, **keeps** its
   tab (R7).
3. **Expect (U4b/R8):** the run row's captured final message —
   ```bash
   $FLY automation runs <id> --output
   ```
   shows the agent's last assistant turn, **secret-scrubbed** and control-sanitized.
   Sanity-check the confidentiality guard: a run whose cwd has >1 transcript modified
   after dispatch records **no** output (abstains) rather than another session's text.

## Cleanup
- Delete the test automation: `$FLY automation delete <id>`.
- If you repointed the hook for Test C, restore `~/.claude/settings.json` from the
  `.bak`.
- The dev flavor's state lives under `~/.local/share/fly-dev/` and
  `~/.config/fly-dev/` — safe to remove to reset.

## Notes for whoever picks this up
- The transcript output capture resolves the run's transcript by **cwd + dispatch
  time** (abstains when >1 qualifies), not a pane-precise session id: the backend
  has no `pane_id → session_id` path (the resume store is keyed by frontend leaf).
  A pane-precise resolver is a possible future refinement — see the code note in
  `session/transcript.rs::sole_transcript_since`.
- Deferred (out of scope, see the plan's Scope Boundaries): editing model/effort on
  an existing automation (delete + recreate for now), auto-closing the alerts-log
  tail husk, a config-tunable auto-close linger, and any sidebar marking of the
  Automations workspace.
