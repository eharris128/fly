# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`docs/plans/` holds the full design (indexed by `docs/plans/README.md`, which
also maps the sibling doc dirs: `docs/brainstorms/` for pre-plan requirements,
`docs/residual-review-findings/` for deferred code-review findings,
`docs/notes/` for one-off evaluations); the code is cross-referenced to it by
ID (see "Conventions"). This file is the primary agent guide; `AGENTS.md` is a
thin pointer to it for non-Claude tools, and `src-tauri/src/hooks/CLAUDE.md`
adds a scoped note at the socket security boundary.

## What this is

**fly** is a Linux desktop "terminal for AI coding agents": real PTY-backed
panes, tabs + splits (one agent per pane), and an attention indicator + OS
notification when an agent needs you. v1 wires **Claude Code** as the attention
source. Stack: **Tauri v2** (Rust backend) + **Svelte 5** (Vite/TS frontend) +
**xterm.js** terminal panes.

## Commands

```bash
pnpm install                  # frontend deps
pnpm tauri dev                # run the app (Vite dev server + cargo run) — use this, not a bare cargo build
pnpm flavor:dev               # run a dev build ALONGSIDE an installed release (see "Stable + dev side by side")
pnpm check                    # svelte-check: type-check the frontend
pnpm test:unit                # vitest: all frontend unit tests
pnpm tauri build --bundles deb   # standalone .deb (skip AppImage — it needs network at bundle time)

cargo test --offline --manifest-path src-tauri/Cargo.toml          # all Rust tests
cargo test --offline --manifest-path src-tauri/Cargo.toml --test hook_auth   # one integration-test file (src-tauri/tests/<name>.rs)
cargo test --offline --manifest-path src-tauri/Cargo.toml <substr> # tests whose name matches <substr>

pnpm vitest run src/lib/keymap.test.ts          # one frontend test file
pnpm vitest run -t "leader"                      # frontend tests matching a name
```

**System deps** (Tauri/WebKitGTK on Ubuntu): `libwebkit2gtk-4.1-dev
build-essential libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
patchelf`.

### Environment gotchas (important — these will bite)

- **cargo + the Bash sandbox**: `index.crates.io` is blocked, so any build that
  resolves a *new* crate hangs. Run new-dependency builds with
  `dangerouslyDisableSandbox: true`; once deps are cached, `cargo
  build/test --offline` works sandboxed. (Caution: `dangerouslyDisableSandbox`
  + a foreground `sleep` in the same Bash call has been seen to abort with exit
  144 — keep sleeps out of sandbox-disabled commands.)
- **Run via `pnpm tauri dev`, never a bare debug `cargo build` binary** — the
  debug binary loads the frontend from the Vite `devUrl`, so standalone it shows
  a blank window. The release build embeds the frontend and runs standalone.
- **Release builds render fine here** (verified on this 24.04 box, Wayland +
  X11). An earlier blank-window issue on WebKitGTK 2.52 was the `crossorigin`
  module-script attribute failing to load over Tauri's custom asset protocol;
  the `fly-strip-crossorigin` Vite plugin in `vite.config.ts` strips it and
  fixes it. If a release build ever shows blank on Wayland, `GDK_BACKEND=x11
  fly` is a proven fallback.
- **Wayland screenshots are locked down** (GNOME 46: `org.gnome.Shell.Screenshot`
  → AccessDenied; x11grab of rootless Xwayland is black). To capture the app,
  launch it with `GDK_BACKEND=x11` (makes it an Xwayland client), then
  `xwd -id <winid> -out f.xwd && ffmpeg -i f.xwd f.png`.
- **The webview console is invisible.** The frontend forwards uncaught errors to
  app stderr via the `frontend_log` command — grep stderr for `[fly-webview]`.

## Packaging & running a stable build

`pnpm tauri build --bundles deb` produces
`src-tauri/target/release/bundle/deb/fly_<ver>_amd64.deb`. Install it
(`sudo apt install ./…deb`) for a standalone `/usr/bin/fly` + launcher,
independent of the source tree.

### Stable + dev side by side

A normal `pnpm tauri dev` shares the installed app's Tauri **identifier**
(`dev.evan.fly`) and its config/session/socket dirs, so the `single-instance`
plugin (registered in `lib.rs`) would just focus the installed window instead of
opening a second one, and the two would clobber each other's saved tabs. To run
an iterating dev build next to an installed stable app, use **`pnpm flavor:dev`**:

- `src-tauri/tauri.dev.conf.json` (merged via `tauri dev --config`) gives the
  dev build a distinct identifier `dev.evan.fly-dev` → its own single-instance
  lock, so both windows coexist.
- `FLY_APP_NAME=fly-dev` (set by the script) isolates its on-disk state. All
  three path roots derive from `lib.rs::app_dir_name()` (default `fly`,
  overridable via `FLY_APP_NAME`): config `~/.config/<app>/`, session/scrollback
  `~/.local/share/<app>/`, and the hook socket under `$XDG_RUNTIME_DIR/<app>/`.
  The installed release leaves `FLY_APP_NAME` unset → stays on `fly`.
- The dev window's title becomes `fly (dev)` (set at runtime in `lib.rs` setup
  when the flavor isn't `fly`) so it's distinguishable from the stable window.

The per-pane hook socket is also PID-keyed, so it never collides regardless.

## Architecture

### One binary, two roles
`main.rs` → `lib.rs::run()`. If argv[1] is a CLI subcommand (`notify`, `hooks`,
`automation`), the process runs as the **`fly` CLI** and exits; otherwise it
launches the Tauri desktop app. The CLI and the app share the same `fly_lib`
crate, so a `fly notify` invocation inside a pane talks to the running app.

`fly automation <create|list|show|runs|pause|resume|run|delete>` (U9) manages
cron-scheduled runs: read ops work anywhere (they read the store file directly),
mutating ops must run inside a pane (they post over the token-authenticated hook
socket). See the Automations module map below.

### The attention pipeline (the core feature, spans many files)
This is the data flow that makes an agent's "I need you" reach the UI:

1. `fly hooks setup` installs a Claude Code `command` hook in
   `~/.claude/settings.json` (backed up first) that runs `fly notify`.
   `fly hooks teardown` removes only fly's hooks.
2. When a pane spawns (`stream::spawn_pane`), the backend mints a per-pane
   CSPRNG token and injects `FLY_PANE_TOKEN` + `FLY_SOCKET_PATH` into the child
   env, and registers the pane with the `AttentionManager`.
3. Claude Code's `Notification`/`Stop` event fires `fly notify`, which connects
   to the **authenticated Unix socket** (`hooks/`): constant-time token compare
   + `SO_PEERCRED` + lockout. This is the security boundary — treat it as such.
4. `HookServer`'s dispatch feeds a `Signal` into `AttentionManager::signal`,
   which runs the pure attention state machine and the **suppression policy**
   (`state/policy.rs`) against the focus/foreground tuple replicated from the
   frontend.
5. On a raise it emits `pane://attention` to the frontend (ring on the pane/tab)
   and, if not suppressed, surfaces an OS notification through the
   `NotificationGate` (coalesce when many panes are raised; rate-limit bursts).

Attention has a **tiered confidence model** (`Tier`: Hook/Cli/Bel/Osc). `Hook`
is Claude Code's own hook (the v1 default); `Cli` is a raise from any `fly
notify` caller — the automations alert path (KTD-H) surfaces a non-silent
script run as `Signal { reason: Reason::Alert, tier: Tier::Cli }`, the first
non-agent attention producer. `Reason::Alert` flows end-to-end (raise → ring →
triage badge, U12/R18); `Bel`/`Osc` remain forward design. Each pane is modeled
by **two orthogonal pure state machines** — `state/lifecycle.rs` (process status)
and `state/attention.rs` (Idle→Raised→Acknowledged) — plus a third pure,
time-injected signal, `state/activity.rs` (the current output "work stretch"
that drives the dashboard's working/idle read and the triage nudge; note the
attention machine has **no** output-driven transition, so "resumed working" must
come from this activity poll, not `pane://attention`). All three take
time/inputs as arguments so they're tested without a running app.

### Backend modules (`src-tauri/src/`)
- `pty/` — `PtyManager` registry + `Pane`: portable-pty, one read thread per
  pane, backpressure pause/resume (watermarks), ordered reap-on-exit.
- `stream/` — `spawn_pane`, raw-byte PTY output over a Tauri `Channel` (no
  transcoding), and the pane↔attention/focus wiring + Tauri commands.
- `state/` — the pure state machines (`lifecycle`, `attention`) + the output
  `activity` tracker + suppression policy (`policy.rs`) + per-pane `manager`.
- `hooks/` — the authenticated socket **(the security boundary)**: `token`,
  `protocol` (the notify path + the `automation/*` request envelope, U9),
  `server`. Has its own scoped `CLAUDE.md` — read it before changing anything here.
- `cli/` — `fly notify`, `fly hooks setup|teardown`, `fly automation …` (U9).
- `automations/` — cron-scheduled agent/script runs (see the module map below).
- `session/` — layout persistence (`mod.rs`) **plus the resume/handoff
  subsystem** (`resume.rs`, `transcript.rs`, `handoff.rs`); see "Session, resume
  & handoff" below.
- `usage/` — live plan-usage snapshot for the dashboard: reproduces Claude Code's
  `/usage` gauges via `GET /api/oauth/usage` (read-only OAuth bearer from
  `~/.claude/.credentials.json`), fetched on dashboard open only (KTD-C), never
  on a timer.
- `feed/` — the loopback HTTP surface for an external local consumer
  (feat-agent-state-local-feed + feed-agent-reply-io + feed-pending-question +
  feed-conversation-tail + feed-question-screen-fallback):
  bearer-token auth (constant-time, silent 401; **the token is the whole
  boundary** — loopback TCP has no `SO_PEERCRED`), SSE `/feed` (webview-pushed
  roster + backend automations; `lastReplyAt` **and** `questionPendingAt`
  stamped at emit), `GET /agents/{key}/output` (latest reply + the gated
  pending `question` object + the `turns` conversation tail — ≤12 turns ×
  ≤2048 chars, oldest→newest ending at the current reply with
  `at == repliedAt`, key omitted when no servable history — all via
  `fallback.rs::FallbackResolver.resolve_io` (wrapping the transcript-pure
  `io.rs::ReplyResolver`) — the ONE source for every per-agent
  surface; question and turn strings are control-sanitized, *then*
  secret-scrubbed, *then* truncated — see `io.rs::clean` for why that order),
  and the single mutation route
  `POST /agents/{key}/input` (submit = control-stripped bracketed-paste +
  Enter; `mode:"keys"` = raw filtered answer keys with **mandatory**
  `ifAskedAt` + a per-leaf answered latch; `mode:"other"` =
  feed-other-answer's free-text answer into the picker's own
  "Type something." row — fly resolves the row's digit from the question's
  `otherKey` and delivers digit → filtered text → Enter as three
  delay-spaced PTY chunks, never a bracketed paste, whose leading ESC would
  cancel an unfocused picker; guarded exactly like keys and refused (409)
  when `otherKey` is unknown; answering a *permission*
  dialog — or any *screen-derived* question while the live reason is
  `permission` — requires `feed.allowPermissionAnswers`, default off). A
  pending interaction is parsed from the transcript tail
  (`session/transcript.rs::pending_interaction_from_str`, backward walk,
  abstain-on-surprise) — **primary but blind at ask time on Claude Code ≥
  2.1.206**, which flushes the pending `tool_use` only when the turn resolves.
  The screen fallback (feed-question-screen-fallback, gate widened by
  fix-feed-question-detection-gaps) covers that gap, strictly behind the
  transcript scan: each pane tees its raw output into a 64 KiB tail ring
  (`pty/pane.rs`); when the transcript abstains, the gate is
  `~/.claude/sessions/<pid>.json` (`session/livestate.rs`), three-valued —
  `waiting` engages the fallback (the attention reason is deliberately NOT
  required: AskUserQuestion fires no hook, and a raise on a visible pane is
  instantly acknowledged, so a blocked pane routinely has no reason);
  explicitly not-waiting abstains; **no entry at all** (a *child-session*
  claude — `CLAUDE_CODE_CHILD_SESSION` in its env — writes no sessions file
  and no transcript; fly strips those markers from pane env at spawn, but a
  pre-existing session may still lack the file) falls through to the screen
  parse as sole authority, engaged only for a non-`working` pane and exposing
  nothing without a fully parsed body. The ring is replayed through a minimal
  `vte` grid and matched against Claude's picker shape (`feed/screen.rs`,
  abstain-on-surprise, digits as rendered — fixtures in
  `tests/fixtures/screen/` are real captured renders).
  Two-tier degrade: a body abstention still stamps `questionPendingAt`
  (tier 1 — corroborated-waiting leg only). A screen-derived body carries
  `source:"screen"` and its `askedAt` is the ask-time raise stamp
  (`feed/pending.rs`, stamped by the hook dispatch) when it postdates the
  corroborator's own stamp, else the sessions file's `statusUpdatedAt` or (on
  the no-entry leg) the ring's last-write time — never a transcript stamp — a
  late transcript flush takes over under its own stamp and stale `ifAskedAt`
  answers 409.
- `notify/`, `config/`, `cwd/` (via `/proc`), `lifecycle.rs` (ordered shutdown —
  reap every pane, no zombies/orphans).
- All Tauri commands are registered in the `invoke_handler!` in `lib.rs`; the
  frontend's typed wrappers for them live in `src/ipc.ts`. Add a command in both
  places.

### Automations (`src-tauri/src/automations/`, cross-referenced U1–U12)
Cron-scheduled runs that either spawn a `claude --dangerously-skip-permissions
"<prompt>"` agent pane (Agent
mode) or run a stored script with no model spend (Script mode). Data flow: a
named `fly-automation-sweep` thread ticks every 10s; a due automation is
**claimed + persisted before it runs** (R2), then dispatched off the store lock
(KTD-B). Modules:
- `model.rs` (U1) — pure domain vocabulary: `Automation`, its bounded run
  history (`RunRow`, R8), and the run-row state machine (claim/skip/close). Serde
  **camelCase** — this shape crosses the store file, the socket, and the
  dashboard, so it is the single wire contract.
- `schedule.rs` (U2) — cron/timezone math (croner), the 5-min min-gap clamp (R1).
- `store.rs` (U3) — the write-through **mutex-authority** store (KTD-B): the
  in-memory map is authoritative, flushed atomically per mutation; `StoreHealth`
  tracks corruption (`.bad.bak` rename, R6) and flush failures for the dashboard.
- `mod.rs` (U4) — `AutomationManager` (create/pause/resume/delete/manual-run),
  the sweep, startup recovery, ordered shutdown; the `list_automations` command
  (U10) and `AutomationsDashboard` DTO. `AUTOMATION_CHANGED_EVENT`
  (`automation://changed`, payload = the automation id) fires after every
  mutation so the dashboard refetches.
- `script.rs` (U5) — the script runner (interpreter enum, timeout, output
  classification → alert vs silent). An alert-classified run hands off through
  the injected `AlertSink` seam (U6 wires the real one).
- `alerts.rs` (U6) — alert surfacing. `AlertsLog` owns the sanitized
  append-only `automation-alerts.log` (R16: `notify::sanitize_*` strips control
  chars incl. newlines at write time, so a script can't forge a log line;
  64 KiB tail-truncated on startup), a bounded **pending queue** for alerts
  arriving before the sink pane exists (R17), and the **sink registry**
  (`register_sink`/`clear_sink_if`). `lib.rs`'s `set_alert_sink` closure (on the
  reaper thread; only this lock + the log file, never the store lock — KTD-B)
  appends then rings the sink pane via `raise_alert` (`Signal { Alert, Cli }` →
  `emit_attention`, R18) or queues + emits `automation://alert-pending`. The
  frontend single-flights a background "Automations" tab that `tail -f`s the log
  and calls the `register_alert_sink` command, which drains the backlog.
- Agent dispatch (U7/U7.5/U8) links run↔pane atomically in `stream::spawn_pane`
  (threading `automation_run_id`), spawns a background ephemeral tab
  (`App.svelte` `handleAgentRun` + `lib/automation-panes.ts`), and closes the run
  on the agent's Stop / pane-exit / 30-min deadline. The **R22 recursion gate**
  blocks an automation-spawned pane from creating or running automations.
- CLI (`cli/automation.rs`, U9): read ops (`list`/`show`/`runs`) read the store
  file directly (work outside a pane); mutating ops (`create`/`pause`/`resume`/
  `run`/`delete`) post over the hook socket (token-validated, origin-stamped,
  R22-gated).
- Dashboard panel (U10): `lib/automations.ts` is the pure view-model
  (`automationsToRows` — sort next-run asc / paused last, mirroring the CLI —
  plus `humanSchedule`/`relativeTime`); `HomeView.svelte` renders it read-only
  below the agent list, with the R6 store-health warning row.

**Dedicated workspace + per-automation model** (`docs/plans/2026-07-03-002-feat-automations-workspace-and-model-plan.md`
— its own U1–U10/R1–R15, scoped per that plan). Two Agent-mode follow-ons layered on the above:
- **Dedicated Automations workspace (U6/U7).** Every agent run *and* the
  alerts-log tab open in one durable workspace marked by a persisted
  `role: "automations"` on `Workspace`/`SavedWorkspace` (`lib/{workspaces,serialize}.ts`).
  Placement resolves by **role, never the in-memory `ws-N` id** (which resets each
  launch): `automation-panes.ts::findAutomationsWorkspace` + `App.ensureAutomationsWorkspace`
  (provision-if-absent, silently recreated after delete). This **replaces** the old
  origin-workspace/first-workspace `resolveTargetWorkspace` placement.
- **Per-automation model + effort (U1–U4a).** `Mode::Agent` and `RunRow` carry
  optional `model`/`effort` (serde `#[serde(default)]`, back-compat); `fly
  automation create` takes `--model`/`--effort` (agent-only; effort ∈
  {low,medium,high,xhigh,max}). `config.automation_defaults` (`AutomationDefaults`:
  `model`/`effort`/`fallback_model="sonnet"`) is the shared default. The manager
  resolves **automation → shared default → Claude default** once per dispatch
  (`resolve_agent_launch`, off the store lock via an injected `Arc<ConfigStore>`),
  stamps the resolved values on the `RunRow` (R13), and rides them on the
  `automation://agent-run` event; `App.buildAgentArgv` appends
  `--model`/`--effort`/`--fallback-model` (prompt last). Dashboard shows them (U9).
- **Auto-close + output capture (U4b/U5/U8).** On agent-run close the manager
  captures the run's **final assistant turn** from its transcript
  (`session/transcript.rs::{last_assistant_text,sole_transcript_since}` — resolves
  by cwd + dispatch-time, abstains when >1 transcript qualifies, a confidentiality
  guard) into `RunRow.output`, **secret-scrubbed** (`automations/redact.rs`) +
  control-sanitized (injected `OutputCapturer` seam, wired in `lib.rs`). The close
  then emits `automation://run-closed {runId, status}` (`RunClosedEmitter` seam);
  the frontend `handleRunClosed` auto-closes a **succeeded** run's tab after a ~6s
  linger (`shouldAutoCloseRun`) and keeps a failed / genuinely-raised one (R7). The
  KTD5 gap: the completion Stop both closes the run *and* raises attention on the
  never-focused pane, so `lib.rs`'s hook dispatch **suppresses the completion
  raise for automation-linked panes** (`is_automation_pane` + `Reason::Finished`) —
  else `succeeded && !isRaised` would never fire.

**Monitors** (`docs/plans/2026-07-10-002-feat-monitor-handoff-plan.md` — its own
U1–U8/R1–R18). A monitor is an agent-mode automation flavor for parked
experiments: a not-before floor (`schedule.rs` clamps every `next_run_at`
recompute) + a sparse recurring cron; each check's captured final turn is
parsed for one fenced ` ```verdict ` block (`automations/verdict.rs` — the
contract text is `VERDICT_BLOCK_SPEC`, quoted verbatim by
`skills/fly-monitor-handoff/SKILL.md`, edited only together; abstain-on-surprise,
so no block = "not done" = silent). A parsed verdict **retires** the monitor in
the same store mutation that closes the row (`retiredAt` set, `next_run_at`
cleared; claims/manual runs refused thereafter); FAIL also writes a durable
bundle file under `<data root>/monitor-bundles/` (outside the run-output tail
cap; evidence itself is tail-capped at 256 KiB at write time) and every verdict
rings via the existing Alert path. Three consecutive *unreadable* checks
(Failed closes, Succeeded closes whose capture abstained, or captures whose
opened ` ```verdict ` fence never parsed — a near-miss block is unreadable, not
a healthy not-done — `Automation::consecutive_infra_failures`, derived not
stored) ring "monitor broken"; readable not-done checks reset. `create
--monitor` captures pickup pointers from the registering pane via the shared
handoff qualification (`session/handoff.rs::resolve_target_now`) or **refuses**
(nothing stored), then emits `automation://monitor-registered` and the frontend
closes the registration residue (no linger — `monitorCloseTarget` in
`lib/automation-panes.ts`: the whole tab only when the registering pane is its
sole leaf, else just that pane's leaf — split siblings are unrelated live
sessions and survive). The dashboard derives
monitor states (parked/paused/broken/retired-pass/retired-fail) mirroring the
CLI's derivation, and a retired-fail row offers the one-action **pickup**:
validate transcript+cwd (`monitor_pickup_check`), spawn a default-permission
recovery session in the current workspace (`lib/handoff.ts::
buildMonitorPickupCommand`, prompt before `--add-dir`), or fall back to showing
the bundle inline (`read_monitor_bundle`, bundle-dir-scoped). Checks fire only
while fly runs; missed ticks are never caught up.

### Session, resume & handoff (`session/` + `lib/{resume,handoff}.ts`)
Durable, backend-owned stores kept **separate** from the debounced layout blob
(`session/mod.rs`), all under the `FLY_APP_NAME` root so a dev flavor stays
isolated. fly only ever **reads** under `~/.claude`; it writes nothing there.
- **Resume** (resume-agents + fix-resume-session-selection plans): `resume.rs` is
  a write-through store mapping each layout leaf → its last `session_id`/`cwd`/
  `argv`, flushed atomically per upsert so an unclean shutdown still leaves the
  mapping on disk; a clean-exit marker (absent at startup ⇒ prior run died
  uncleanly) drives the crash auto-offer. `transcript.rs` derives the session id
  straight from Claude's transcript filenames
  (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`), so capture doesn't depend on
  the installed `fly` binary's wire version. `lib/resume.ts` builds the exact
  replay argv (stripping stale `--resume`/`--continue` and one-shot positional
  prompts — the flag hygiene lives in this one tested place).
- **Handoff** (session-handoff plan): `handoff.rs` resolves a *stale* leaf's
  previous session — from the durable resume record, **not** the 15-min-recency
  live id — into a spawnable `HandoffTarget`, qualified by at least one real
  transcript turn. `lib/handoff.ts` (see Frontend) drives the chords and the
  guided-injection state machine.
- **Attribution** (fix-session-pane-attribution plan): a resume record's session
  id is trust-ranked `Poll < Hook < Pick`. A capture-only `SessionStart` hook
  (`fly notify --claude --capture`, installed by `fly hooks setup`) stamps
  pane-precise ids over the socket without raising attention; the poll abstains
  when >1 fresh session shares a cwd (`transcript.rs::active_session_for_cwd`);
  an ambiguous handoff routes through the session pick-list
  (`lib/session-picker.ts` + `SessionPicker.svelte`), and an explicit pick is
  remembered at the highest rank — a divergent hook never rebinds it, only sets
  a re-pick prompt flag. **Corroborate-then-remember**: a quick (unattended,
  bypass-permissions) handoff only fires zero-prompt against a remembered
  `Pick`; an uncorroborated Hook/Poll target lists once and the pick persists.
  `leader g` resets a leaf's attribution and forces a re-pick (the escape
  valve for a stale or mis-attributed id). The `SessionStart` contract this
  rests on is **empirically confirmed** (Claude Code 2.1.200): the hook inherits
  `FLY_PANE_TOKEN`, carries `session_id`/`transcript_path`/`cwd`/`source`, and
  `/clear` rotates to a distinct id (hook→hook rotation holds). Caveat: a plain
  `claude` in an untrusted dir may not flush a transcript, so the **resume store,
  not the transcript file, is the reliable capture signal** when verifying.

### Agent dashboard & attention triage (frontend + `state/activity.rs`, `usage/`)
- **Dashboard / "home"** (`leader d`; agent-dashboard + dashboard-home-base +
  running-state plans): `lib/home.ts` is the pure view-model — it folds App's
  live `agentByLeaf`/`attentionByLeaf`/activity maps into grouped agent rows with
  a `waiting`/`working`/`idle`/`running` status precedence (only `isAgent` panes
  become rows; empty ⇒ the R7 empty state). `HomeView.svelte` renders it, plus
  the read-only automations panel and the `usage/` gauges. The working/idle
  signal is `state/activity.rs`; the `running · N tasks` count is the `/proc`
  task probe (top-level pgids only — see the `dashboard-running-state` memory).
- **Attention triage** (reason-typed-triage + dashboard-home-base plans):
  `lib/notifications.ts` + `NotificationPanel.svelte` are the notification
  history (`leader n`, keyed by **leafKey** so it survives paneId reassignment;
  clear removes the entry). `lib/nudge.ts` + `NudgeOverlay.svelte` are the
  "handled — move along" nudge (Tab rotates to the next agent / dashboard). Both
  are pure & framework-free like `home.ts`; the nudge takes **no** DOM focus
  (HotkeyMenu archetype) so type-through never drops a keystroke.

### Frontend (`src/`)
- `App.svelte` — orchestrates workspaces, tabs, and the split tree; owns
  attention/cwd/activity state, debounced session persistence (~800ms), and the
  overlay wiring (hotkey menu, command palette, notification panel, triage nudge,
  dashboard, destructive-confirm).
- `lib/layout.ts` — **pure split-tree model**. Leaves render flat and keyed, so
  splitting/resizing never unmounts a pane (which would respawn its agent). Leaf
  keys are stable and also key the scrollback files — preserve this invariant.
  `App.svelte` renders every pane across **all** workspaces/tabs (hiding inactive
  ones) so switching never unmounts/respawns an agent — same invariant.
- `lib/workspaces.ts` — **pure workspace/tab model** (mirrors `layout.ts`): a
  workspace is a named collection of tabs; helpers (`tabDisplayTitle`,
  `closeTabIn`, `deleteWorkspaceFrom`, `flattenRaised`) take id factories so
  they're tested without an app.
- `lib/keymap.ts` — the leader-key model (tmux-style: default Ctrl-A, then a
  command key; everything else passes through to the PTY). `BINDINGS` is the
  single source of truth shared by `dispatch()`, the hotkey menu, and the
  command palette, so they cannot drift.
- `lib/Terminal.svelte` — embeddable xterm leaf; subscribes to `pane://attention`.
  Terminal font size comes from config (`config.fontSize`, default 15).
- `lib/Sidebar.svelte` — collapsible cmux-style workspace tree (workspaces ▸
  named tabs); `lib/ControlBar.svelte` — slim top bar (sidebar toggle +
  breadcrumb + pane controls).
- `lib/{config,serialize}.ts` (`serialize.migrateSession` upgrades old sessions
  into the workspace shape), `lib/HotkeyMenu.svelte` (passive cheat-sheet).
- `lib/CommandPalette.svelte` + `lib/palette.ts` — type-to-run command palette
  on `leader p`: every `BINDINGS` action (so it can't drift) plus live
  jump-to-workspace/tab navigation. Unlike the cheat-sheet it takes DOM focus,
  so `App.focusActivePane()` hands focus back to the active pane on close.
- `lib/handoff.ts` — session handoff (`leader f` quick / `leader F` guided,
  U1–U4 of the session-handoff plan): a stale pane's previous session is
  resolved from its durable resume record (backend `session/handoff.rs`) and
  handed to a fresh `claude` in a split alongside. The pure module builds the
  argv — prompt positional **before** the variadic `--add-dir` (which would
  swallow a trailing one) — and houses the guided-injection state machine
  (spawned→ready→injected; user-typed-first/timeout→skipped, exit→cancelled)
  that pre-types the pickup prompt unsent via bracketed-paste `pty_write`.
  Handoff panes are ordinary panes: no automation linkage;
  `resume.ts::sanitizeFlags` strips positionals so a restart never re-fires the
  prompt. Quick launches bypass-permissions (`--dangerously-skip-permissions`,
  since it runs the pickup prompt unattended); guided stays default permission
  mode (the user reviews the pre-typed prompt before sending). A quick launch
  is gated on corroboration — zero-prompt only against a remembered `Pick`,
  one forced pick-list pass otherwise (see Attribution above). `leader g`
  (fix-session-pane-attribution U8) resets the pane's attribution and re-runs
  quick handoff with the pick-list forced.

## Conventions

- Code is cross-referenced to the design by ID — **KTD\<n\>** (key technical
  decision) and **R\<n\>**/**U\<n\>** (requirement/unit) appear in doc comments
  and tie back to `docs/plans/`. IDs are **scoped per plan** — each plan restarts
  its KTD/R/U numbering, so `KTD7`/`R10`/`U8` mean different things in different
  plans; resolve an ID against the plan the file belongs to
  (`docs/plans/README.md` maps each plan to its code). When changing behavior, keep the
  referenced IDs accurate. Match the surrounding style; modules are heavily
  doc-commented.
- Behavior-bearing units ship with tests (Rust state machines are test-first and
  pure; frontend has vitest for layout/keymap).
- Commits: conventional, with a `Co-Authored-By: Claude` trailer.
