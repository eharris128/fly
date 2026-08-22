# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`docs/plans/` holds the full design (indexed by `docs/plans/README.md`, which
also maps the sibling doc dirs: `docs/brainstorms/` for pre-plan requirements,
`docs/residual-review-findings/` for deferred code-review findings,
`docs/notes/` for one-off evaluations); the code is cross-referenced to it by
ID (see "Conventions"). This file is the primary agent guide; `AGENTS.md` is a
thin pointer to it for non-Claude tools, and `src-tauri/src/hooks/CLAUDE.md`
adds a scoped note at the socket security boundary. `README.md` is the
human-facing orientation layer — when a change moves the product story (shell,
install path, CLI surface, headline features), update it too; it goes stale
silently otherwise. `electron/README.md` covers the shipped shell's dev loop
and packaging.

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
pnpm tauri build --bundles deb   # TAURI .deb (kept buildable — KTD9 rollback; skip AppImage, needs network)
pnpm build:local              # Tauri .deb on the fast release-dev profile (thin LTO — ~2× faster rebuilds)
pnpm build:mac                # on a Mac: .app + .dmg on the release-dev profile (best-effort Tauri target — see docs/macos-build.md)

# The ELECTRON shell (migration plan 2026-08-12-002; the packaged product as of U7):
pnpm build && cargo build --release --offline --manifest-path src-tauri/Cargo.toml \
  && (cd electron && npm run dist)   # → electron/dist-el/fly-electron-shell_<ver>_amd64.deb
                                     # (deb Package: fly — installing it REPLACES the Tauri deb;
                                     #  postinst SUIDs chrome-sandbox + symlinks /usr/bin/fly)
# Electron dev loop (fly-el flavor beside the installed release):
#   pnpm dev   +   (cd electron && DISPLAY=:1 FLY_APP_NAME=fly-el \
#     FLY_SHELL_URL=http://localhost:1420 ./node_modules/.bin/electron . --no-sandbox)
#   (--no-sandbox is dev-only: the repo checkout lacks the SUID helper; the packaged app runs sandboxed)

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

A dev build left on the default flavor shares the installed app's
config/session/socket dirs, so it would clobber the installed app's saved tabs
and fight it over the hook socket. (The installed Electron shell holds its own
single-instance lock in `electron/main.js`; the Tauri `single-instance` plugin
in `lib.rs` only guards the Tauri rollback shell — the real collision today is
the shared `FLY_APP_NAME` state, not the window lock.) To run an iterating dev
build next to an installed stable app, use **`pnpm flavor:dev`** (Tauri) or the
`fly-el` Electron loop from Commands above:

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

The hook socket lives at a stable per-flavor path (`hook.sock` under the
runtime dir — tmux-substrate U2/KTD8: substrate sessions outlive the process,
so surviving agents' env must keep pointing at a live socket across restarts);
flavors get distinct dirs, same-flavor duplicates are stopped by the
single-instance plugin, and the bind refuses to steal a socket that still
answers.

## Architecture

### One binary, three roles
`main.rs` → `lib.rs::run()`. If argv[1] is a CLI subcommand (`notify`, `hooks`,
`automation`, `agents`, `send`, `substrate-event`, `core`, `help`/`--help`/`-h`
— see `cli/mod.rs::is_cli_subcommand`), the process runs as the **`fly` CLI**
and exits; of those, `fly core` runs the **headless backend** (the full
`build_backend` stack served over the control socket — what the Electron shell
spawns and drives) and `fly substrate-event` is the tmux run-shell hook
endpoint. `fly resume` (`lib.rs::resolve_launch_mode`) is a launch-mode arg for
the desktop role, not a CLI subcommand. Otherwise it launches the **Tauri
desktop app** (the KTD9 rollback
shell — since the 2026-08-12 cutover the shipped product is the Electron
shell in `electron/` + `fly core`, and the installed launcher never invokes
the Tauri role). All roles share the same `fly_lib` crate, so a `fly notify`
invocation inside a pane talks to whichever backend is running.

`fly automation <create|update|list|show|runs|pause|resume|run|delete>` (U9)
manages cron-scheduled runs: read ops work anywhere (they read the store file
directly), mutating ops must run inside a pane (they post over the
token-authenticated hook socket). See the Automations module map below.

### The attention pipeline (the core feature, spans many files)
This is the data flow that makes an agent's "I need you" reach the UI:

1. `fly hooks setup` installs Claude Code `command` hooks in
   `~/.claude/settings.json` (backed up first) that run `fly notify`:
   `Notification`/`Stop` (attention), `SessionStart` (capture-only), and
   `PermissionRequest` (matcher `*` — the held ask channel, see `feed/` below).
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

### The tmux session substrate (built 2026-08-11/12, behind a flag)

`config.substrate: "pty" | "tmux"` (default `pty` until live validation —
see `docs/plans/2026-08-11-001-tmux-substrate-LIVE-CHECKLIST.md`) selects
what backs a pane. Under `tmux`, every leaf-keyed pane is a **marked session
on a fly-owned per-flavor tmux server** (`substrate/` — wrapper with executor
seam, injective naming, durable leaf⇄session⇄token store): output streams
through a `pipe-pane` FIFO into the same sink/activity/ring machinery, input
ships as binary-safe `send-keys -H` through a persistent control-mode client
(the unmarked `flyctl-input` session; ~µs/key vs ~8 ms/key subprocess —
fire-and-forget, subprocess fallback on client death), exits arrive via
`pane-died` hooks over the socket (KTD12 server-scope token, persisted for cross-instance
continuity) with a 1.5 s poll floor, and **sessions outlive fly**: quit
detaches (ephemeral automation/sink panes are killed instead), restart
adopts — same child pid, stored pane token re-registered, ~2k lines
replayed. `leader t` opens the focused session in a real terminal
(`config.terminal`); an attached session counts as focused-elsewhere for
suppression. Substrate-independent but born of the same plan:
`config.mirrorUnfocused` (default OFF since the Electron cutover — U8
measured Chromium at ~14% renderer main-thread on the 5-pane flood with or
without it; `true` restores it for the Tauri/WebKitGTK rollback, where it
was the engine-floor relief: 63% → 4–14% webview main-thread) renders
visible-unfocused panes as 2 Hz DOM snapshots of their hidden xterm
buffers (`lib/mirror.ts`). Plan:
`docs/plans/2026-08-11-001-feat-tmux-session-substrate-plan.md`.

### Backend modules (`src-tauri/src/`)
- `pty/` — `PtyManager` registry + `Pane`: portable-pty, one read thread per
  pane, backpressure pause/resume (watermarks), ordered reap-on-exit.
- `stream/` — `spawn_pane`, raw-byte PTY output over a Tauri `Channel`, and the
  pane↔attention/focus wiring + Tauri commands. ("No transcoding" (KTD3) is
  **lossless but not literal**: tauri 2.11.3 re-encodes a `Raw` chunk under
  1024 bytes as a JSON number array in an `eval()` — exact bytes, ~3.4× wire
  cost. See the foundation plan's 2026-07-28 KTD3 addendum.) `coalesce.rs`
  (performance-audit T1, landed 2026-08-04) batches PTY reads per pane before
  the channel on a **visibility-aware deadline** — ~4 ms visible, ~250 ms
  hidden (retuned by `set_visible_panes`), 64 KiB size trip — so interactive
  repaints ride the ≥ 1 KiB raw path instead of one eval per read, and hidden
  panes cost ~4 main-thread wakeups/s each under flood.
- `state/` — the pure state machines (`lifecycle`, `attention`) + the output
  `activity` tracker + suppression policy (`policy.rs`) + per-pane `manager`.
- `hooks/` — the authenticated socket **(the security boundary)**: `token`,
  `protocol` (the notify path + the `automation/*` request envelope, U9, + the
  newline-framed held `ask/hold` op, hook-ask-channel, + the `peer/*`
  request/response ops, agent-peer-messaging), `server`. Has its own
  scoped `CLAUDE.md` — read it before changing anything here.
- `cli/` — `fly notify`, `fly hooks setup|teardown`, `fly automation …` (U9),
  and `fly agents` / `fly send` (`cli/peer.rs`, see the `peer/` bullet above).
- `automations/` — cron-scheduled agent/script runs (see the module map below).
- `session/` — layout persistence (`mod.rs`) **plus the resume/handoff
  subsystem** (`resume.rs`, `transcript.rs`, `handoff.rs`); see "Session, resume
  & handoff" below.
- `usage/` — live plan-usage snapshot for the dashboard: reproduces Claude Code's
  `/usage` gauges via `GET /api/oauth/usage` (read-only OAuth bearer from
  `~/.claude/.credentials.json`), fetched on dashboard open only (KTD-C), never
  on a timer. `usage/gate.rs` is the automations **usage gate** (usage-limit-
  deferral plan): the pure at-limit predicate plus a short-timeout, TTL-cached
  blocking fetch over the same request core — consulted by the sweep only when
  an agent-mode claim is possible, so KTD-C's no-timer rule still holds.
- `feed/` — the loopback HTTP surface for an external local consumer
  (feat-agent-state-local-feed + feed-agent-reply-io + feed-pending-question +
  feed-conversation-tail + feed-question-screen-fallback + hook-ask-channel +
  feed-monitor-enrichment + phone-screenshot-drop):
  bearer-token auth (constant-time, silent 401; **the token is the whole
  boundary** — loopback TCP has no `SO_PEERCRED`), SSE `/feed` (webview-pushed
  roster + backend automations; `lastReplyAt` **and** `questionPendingAt`
  stamped at emit; the automation projection additionally carries `monitor`,
  `retiredAt`, and `lastVerdict {outcome, note}` — additive, absent-field
  back-compat — because a verdict parses only from an infra-clean run, so a
  monitor's FAIL rides a `lastStatus:"succeeded"` row and a consumer must read
  the verdict, not run status, for honest pass/fail; see
  `docs/notes/2026-07-16-feed-monitor-enrichment.md`),
  `GET /agents/{key}/output` (latest reply + the gated
  pending `question` object + the `turns` conversation tail — ≤12 turns ×
  ≤2048 chars, oldest→newest ending at the current reply with
  `at == repliedAt`, key omitted when no servable history — all via
  `fallback.rs::FallbackResolver.resolve_io` (wrapping the transcript-pure
  `io.rs::ReplyResolver`) — the ONE source for every per-agent
  surface; the reply `text` is control-sanitized *then* secret-scrubbed
  (never truncated — audit-remediation U1), and question and turn strings are
  control-sanitized, *then* secret-scrubbed, *then* truncated — see
  `io.rs::clean` for why that order),
  and the per-agent mutation route
  `POST /agents/{key}/input` (submit = control-stripped bracketed-paste +
  Enter — refused 409 `askPending` when unguarded while a permission ask is
  pending, audit-remediation U2; `mode:"keys"` = raw filtered answer keys with **mandatory**
  `ifAskedAt` + a per-leaf answered latch; `mode:"other"` =
  feed-other-answer's free-text answer into the picker's own
  "Type something." row — fly resolves the row's digit from the question's
  `otherKey` and delivers digit → filtered text → Enter as three
  delay-spaced PTY chunks, never a bracketed paste, whose leading ESC would
  cancel an unfocused picker; guarded exactly like keys and refused (409)
  when `otherKey` is unknown; `mode:"decision"` = hook-ask-channel's
  `{"decision":"allow"|"deny"}` against a HOOK-sourced permission ask,
  resolved through the held connection — never PTY bytes; answering a
  *permission* dialog (any mode incl. decision) — or any *screen-derived*
  question while the live reason is `permission` — requires
  `feed.allowPermissionAnswers`, default off).
  **The phone screenshot drop** (phone-screenshot-drop) adds the second
  mutation route and the feed's only HTML: `POST /drop?agent=&pane=&caption=`
  takes the image as the **raw body** (multipart would mean a boundary parser
  in the security-sensitive listener; the caption rides the query because a
  header cannot hold a newline or emoji unencoded), and `GET /` — plus the
  equivalent alias `GET /drop-page`, the two paths the one branch answers —
  serves an inert `include_str!`-embedded page (`feed/drop-page.html`)
  **unauthenticated** —
  the second and last exception to auth-precedes-routing after `/healthz`,
  unavoidable because a browser navigation sends no `Authorization` header, and
  safe because the shell carries no roster/agent data/token (pinned by a
  byte-identical-across-rosters test). The phone reaches it through
  `tailscale serve` in front of the same loopback port; fly adds **no** bind
  (`the_drop_routes_add_reachability_without_adding_a_bind` guards that).
  `feed/drop.rs` owns sniffing (bytes decide the format, never the declared
  content type; extension from a fixed literal set, filename fully fly-minted),
  streaming storage (temp-then-rename, 0600 in 0700, unlink-on-drop so a
  refusal leaves no residue, startup sweep for crash residue), the prompt
  composition, and `deliver_with_guards`. That last owns the whole ordering
  because it is load-bearing: guards → **publish** → paste → re-probe → Enter.
  The image must be published after every refusal check (residue) but before
  any text reaches the pane (the agent is about to read that path). Two
  check-then-act guards, honestly bounded: pane **identity** (`paneId` echoed
  from the roster — ids are monotonic and never reused, so this is identity not
  freshness, unlike the stamp that burned `feed-askedat-restamp-409`) and a
  `/proc` foreground probe re-run before the Enter, since `claude` exiting mid
  paste-to-Enter would otherwise **execute** the caption in a shell. The
  question gate is **wider here than on the input route** — *any* pending
  question refuses, because `paste_payload`'s leading ESC silently cancels an
  unfocused picker; the input route is deliberately unchanged. Every refusal
  drains the body first: `tiny_http` otherwise drains at drop time through a
  buffer sized from the *client-declared* `Content-Length`
  (`equal_reader.rs`), so responding early defers the read and sizes an
  allocation from an attacker's number. Knobs: `feed.dropDir` (raw string, tilde
  expanded at use — never at deserialization, or `set_config`'s round-trip
  would persist an absolute path over the user's `~`), `feed.dropMaxBytes`,
  `feed.expectedTailnetLogin` (additive identity check, **off by default**;
  a local process can forge the header, so the token stays the boundary).
  **Operator setup lives in
  `docs/notes/2026-07-24-phone-drop-live-check.md` ("Operator setup (U8)")** —
  three required steps, two of which fail silently: the `tailscale serve --bg`
  mount, where to read `feed.token` (no in-app surface shows it), and setting
  `expectedTailnetLogin` (the check ships inert). `tailscale funnel` must never
  appear anywhere near this feature.
  **Pending-question detection is now event-first** (hook-ask-channel): the
  `PermissionRequest` hook fires at ask time for every dialog — incl.
  AskUserQuestion, incl. bypassPermissions (live-verified 2.1.207) — and
  `fly notify --permission-request` forwards a bounded typed subset over the
  socket as a **held** `ask/hold` request whose connection lifetime IS the
  ask's lifetime (Claude kills the hook when the dialog resolves locally →
  drop clears `feed/ask.rs::AskRegistry`; leaf-keyed, last-write-wins,
  capped). A held ask is the resolver's primary leg (`source:"hook"`,
  `askedAt` = fly's receipt stamp, exposed with NO attention-reason
  corroboration — the held connection is the proof), ahead of the transcript
  walk, and its register/clear both `FeedState::bump` so frames move without
  a roster change. Everything below is the fallback chain, unchanged when no
  ask is held: a pending interaction is parsed from the transcript tail
  (`session/transcript.rs::pending_interaction_from_str`, backward walk,
  abstain-on-surprise) — **primary-after-hook but blind at ask time on Claude
  Code ≥ 2.1.206**, which flushes the pending `tool_use` only when the turn
  resolves. The screen fallback (feed-question-screen-fallback, gate widened
  by fix-feed-question-detection-gaps) covers that gap, strictly behind the
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
- `peer/` — agent peer messaging (agent-peer-messaging plan, U1–U8): `fly
  agents` (`peer/list`) and `fly send <pane> <message>` (`peer/send`) as
  `peer/*` ops on the hook socket beside `automation/*` — same
  token/peer-cred/bounds boundary, sender identity always the
  **token-resolved** pane (KTD2; the wire carries no "from", and `PeerRequest`
  must never gain a `reason` field — that absence is the version-skew rule).
  `peer/mod.rs::dispatch_peer_op` is the pure gate sequence (KTD9, order
  pinned by test): selfSend → tooLong → resolve target → **rosterStale**
  (nothing roster-derived is trusted past a ~10s-old publish stamp; the read
  op serves stale *marked*, the write path refuses) → **notOptedIn** (KTD6:
  per-pane receive consent, **default off, session-scoped, human-only** — the
  dashboard row's "peers" toggle is the sole writer, pushed on the roster as
  `AgentEntry.peerOptIn`; no socket/CLI/config surface can set it, so a
  prompt-injected agent can't opt itself in) → **rateLimited**
  (`rate.rs` token buckets, per-sender + global backstop — the enforceable
  fan-out brake; hop counters were rejected as unenforceable across a model
  turn) → **askPending** (the drop route's own wide `drop_blocked_by_question`
  predicate — any pending question refuses, ESC-cancels-picker hazard) →
  delivery via `feed::drop::deliver_with_guards` (no-op commit, one app-wide
  delivery mutex so two concurrent sends can't splice a composer line).
  `compose.rs` sanitizes (`feed::io::clean` order) then wraps the body in a
  fly-minted UNTRUSTED-provenance frame (delimiter-collision lines rewritten;
  marking is advisory — containment is consent+rate+visibility). Peer
  messaging depends on the webview roster publisher (runs when `feed.enabled`,
  the default, or the dashboard is open) but NOT on the feed listener port.
  Tests: `tests/peer_send.rs` + peer cases in `hook_auth`/`hook_ask`.
- `backend.rs` — **the shell-agnostic backend builder** (Electron-shell
  migration U3.5): `build_backend(seams)` constructs everything `lib.rs`'s
  setup used to wire inline — the hook server's dispatch closure (the
  attention pipeline's policy/notify/resume-capture/feed-bump step), the
  ask/peer/automation/substrate handlers, the automations manager + sweep +
  alert surfacing, and the feed listener — against two injected seams:
  `events` (`app.emit` under Tauri; control-socket broadcast under `fly
  core`) and `banner` (notification plugin vs `notify-send`). `lib.rs` setup
  is now seam construction + `.manage()` of the returned `Backend` fields;
  `fly core` boots the identical backend. Change backend wiring HERE, never
  by re-inlining into a shell.
- `control/` — the core control socket (Electron-shell migration plan
  `2026-08-12-002`, U1): the transport the Electron shell uses to drive the
  headless fly backend (`fly core`). Same-uid peer-cred gate + never-steal
  bind (the `hooks/` discipline, reused), length-prefixed frames with
  JSON-free pane-output/input kinds (kills the KTD3 eval quirk), request/
  response/event envelopes whose `cmd`/`event` names are exactly the Tauri
  seam's, plus the built-ins `core/ping` and `core/shutdown` (the shell's
  ordered-quit trigger). Wire contract in `docs/core-protocol.md` — edited
  only together with this module. `registry.rs` (U2/U3): **all 46** commands
  ported name-identically over the real managers — where a Tauri command body
  holds real logic it was extracted into a shared fn used by both shells
  (`pty::pane_activity_snapshot`, `stream::attach_pane_now`,
  `automations::{dashboard_snapshot,read_bundle_for}`,
  `stream::attention_event_payload`) so they cannot drift. The pane
  lifecycle is fully socket-served — `spawn_pane`'s body is the shared
  `stream::spawn_pane_with` (`SpawnDeps` + per-pane `PaneByteSink`), output
  rides the 0x02 binary frames, keystrokes ride 0x03 down (write +
  attention-clear, `pty_write` parity), exits fan out as `pane://exit` via
  the shared payload builders — and `adopt_live_pane` (renderer-crash
  recovery, 2026-08-22) re-binds a reloaded renderer to the **live** pane
  the core still owns for a leaf (`stream::adopt_live_pane_with`: same id,
  token, attention; the pane's grid + 64 KiB tail returned, the xterm
  sized to that grid before the replay), so an Electron renderer reload re-attaches instead of respawning
  on either substrate — the Tauri arm of that command answers `None` (a
  per-spawn `Channel` can't be re-bound); `fly core` resolves its flavor's launch mode
  (consuming the clean-exit marker, KTD-G) and boots the **full** backend
  through `backend::build_backend` — hook server (with the real dispatch),
  automations + sweep, feed listener — so the complete command surface is
  live headless. The shell itself (window, core spawn/adopt, single-instance
  lock, socket bridge) shipped as U4–U7 in `electron/`; the Tauri shell
  behaves identically through the same builder.
- `config/` — the camelCase `config.json` store. **Sparse persistence** (since
  2026-08-18, `config/mod.rs::{sparse_value,prune_equal}`): the file records
  only divergences from `Config::default()` and preserves unknown keys, so
  don't expect (or write) a full dump on disk. Beyond the keys covered
  elsewhere in this file (`substrate`, `mirrorUnfocused`, `terminal`,
  `fontSize`, `renderer`, `feed.*`, `automationDefaults.*`), `schema.rs` also
  holds: `leaderKey`, `attentionDebounceMs` (400), `nudgeIdleMs`,
  `notificationCoalesceThreshold`, `oscBelFallback`, `scrollbackLines`
  (10k), `saveScrollback`, `showNotificationsIcon`,
  `notificationsMutedDefault`, `notificationSound`, `reasonEffects`, and
  `feed.port` (4939). Two are security-relevant: **`notificationCommand`**
  (opt-in, default off — runs an arbitrary user command on every surfaced
  notification; env/quoting contract in `notify::command`) and
  **`resumeDefaultArgs`** (defaults to `--dangerously-skip-permissions` —
  the flag floor replayed when resuming an agent whose argv wasn't captured,
  so the permission posture isn't silently lost on resume; it means an
  uncaptured resume runs permission-free by design).
- `notify/`, `cwd/` (via `/proc`), `lifecycle.rs` (ordered shutdown —
  reap every pane, no zombies/orphans).
- All Tauri commands are registered in the `invoke_handler!` in `lib.rs`; the
  frontend's typed wrappers for them live in `src/ipc.ts` (except the
  config/session ones, which live next to their models in `lib/config.ts` /
  `lib/serialize.ts`, and `spawn_pane`, whose wrapper is `lib/transport.ts::
  spawnPaneWithSink`). **A new command goes in THREE places**: the
  `invoke_handler!`, the frontend wrapper, and `control/registry.rs` — a
  command missing from the registry works in Tauri dev but is unreachable in
  the packaged Electron app.

### Automations (`src-tauri/src/automations/`, cross-referenced U1–U12)
Cron-scheduled runs that either run a `claude` agent (Agent mode) or a stored
script with no model spend (Script mode). **Agent runs dispatch closed-loop
headless by default**
(`docs/plans/2026-07-31-001-feat-headless-agent-automations-plan.md`): a
backend-owned `claude -p --output-format stream-json` child via the monitor
runner (`headless.rs`) — no pane, no tab, no `automation://agent-run`; one
prompt in, one captured result out (`RunRow.output` from the stream `result`,
`RunRow.session_id` from `init` — the debugging handle `fly automation runs`
prints, `-v` adds the derived transcript path). The disposition resolves
automation-explicit → `config.automation_defaults.headless` (ships **true**;
flipping the knob flips every non-explicit automation at once) at claim time,
in one place — `Mode::resolved_headless`, stamped by `Automation::claim` onto
the row, and dispatch routes on the **claimed row's marker**, so marker and
routing can't disagree. `create --headless`/`--paned` pin one automation
either way (mutually exclusive; agent-only; both rejected with `--monitor`,
which is unconditionally headless). A **Failed** headless close rings the
Automations alert path (sanitized `name: error (run id)` line — the
replacement for the pane path's kept-open failed tab); Succeeded closes are
silent, and a non-monitor run **never retires** — retiring is the monitor-only
half, and it is now the load-bearing distinction, because a non-monitor
Succeeded close *does* verdict-parse when the automation opted in
(fly-dag-primitives G1's `Automation.verdict_gated`, `model.rs`): the fenced
block is parsed out of the run's captured output and stamped on the row in the
same mutation as the close, for a dependent to read. Gating is **explicit
opt-in, never inferred from a block's presence** (G1 KTD1) — ungated, every
pre-G1 automation closes byte-identically. The dashboard panel
row is a running headless run's primary surface (`running · 2m` elapsed read;
effective disposition resolved from the DTO's `headlessDefault`), and the
feed's `AutomationEntry` carries an additive effective-`headless` bool. The
**pane path survives only behind an explicit `--paned`** (or the knob off):
ephemeral tab, transcript retry-capture, KTD5 suppression, kept-failed-tab —
all unchanged there, removal deferred. Data flow: a
named `fly-automation-sweep` thread ticks every 10s; a due automation is
**claimed + persisted before it runs** (R2), then dispatched off the store lock
(KTD-B). A due **agent-mode** occurrence is usage-gated first
(`docs/plans/2026-07-16-001-feat-automations-usage-limit-deferral-plan.md`):
when the plan reads confidently at a session/weekly limit (`usage/gate.rs` —
fail-open on every uncertainty, incl. active overage billing), the sweep
records a pre-claim `Skipped("usage limit")` row and defers `next_run_at`
through the existing `advance_from` floor to the first occurrence at-or-after
the window's `resets_at`; scripts and manual runs are never gated, an
unattended retry skip-closes instead (retry-once), and the knob is
`config.automation_defaults.usage_gate` (default on). Note the pane-mode
at-limit close still reads `Succeeded` (Stop fires regardless) — the honest
reclassification is the plan's deferred U6, gated on an empirical pin the
next time the account is actually at a limit. Modules:
- `model.rs` (U1) — pure domain vocabulary: `Automation`, its bounded run
  history (`RunRow`, R8), and the run-row state machine (claim/skip/close). Serde
  **camelCase** — this shape crosses the store file, the socket, and the
  dashboard, so it is the single wire contract.
- `schedule.rs` (U2) — cron/timezone math (croner), the 5-min min-gap clamp (R1).
- `depend.rs` — **automation dependencies**
  (`docs/plans/2026-08-07-002-feat-automation-dependencies-plan.md`, its own
  U1–U7/KTD1–KTD8): `fly automation create --after <id> [--within <dur>]`
  makes a dependent's cron occurrence a *precondition-gated* fire — the pure
  predicate here decides one of **four**: `Satisfied` (a fresh, successful,
  not-yet-consumed upstream run exists; a success carrying a FAIL *or* a
  DECLINED verdict doesn't count) / `Wait` (window open — the sweep leaves the occurrence untouched and re-evaluates each
  tick, so the dependent fires within ~10 s of the upstream's success) /
  `Withhold` (window closed — a new born-terminal `RunStatus::Withheld` row
  records the specific reason: upstream failed/skipped/stale/still
  running/missing/already consumed, chain-propagating for A→B→C) /
  `Declined` (fly-dag-primitives G1 KTD3/KTD4 — the window closed because the
  upstream's newest in-window terminal run *declined*: it ran and correctly
  had nothing to do. Records a silent born-terminal row and does **not** ring
  the alert — a correct no-op is not a failure — and chain-propagates, so a
  further dependent declines in turn). One
  symmetric `within` window (default 60 min) bounds both staleness and wait.
  Exactly-once per upstream run: the claim stamps `RunRow.upstreamRunId` in
  the same store mutation (dependent retries bypass the gate and inherit
  it). Scheduled withholds ring the Automations alert path; manual runs
  evaluate the same predicate and report the refusal synchronously (no
  override flag — re-run the upstream instead). Create-time validation walks
  the chain (depth ≤ 8, cycle-reject; edges are set at create only — update
  may only *clear* one — so cycles are otherwise unconstructible); upstream
  must exist and not be a monitor.
  Feed projection is additive (`after`, `lastWithheldReason`,
  `lastStatus:"withheld"`); dashboard/CLI render `after:<id>` tags and
  `waiting on upstream`. Back-compat note: the new `"withheld"` status value
  makes a post-withhold store unreadable to *older* fly binaries (`.bad.bak`
  degrade) — accepted, no downgrade path.
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
- **Paned** agent dispatch (U7/U7.5/U8 — since the headless-agent-automations
  plan reachable only via `--paned`/knob) links run↔pane atomically in `stream::spawn_pane`
  (threading `automation_run_id`), spawns a background ephemeral tab
  (`App.svelte` `handleAgentRun` + `lib/automation-panes.ts`), and closes the run
  on the agent's Stop / pane-exit / 90-min deadline (`RUN_DEADLINE_MS`, raised
  from 30 on 2026-08-07). The **R22 recursion gate**
  blocks an automation-spawned pane from creating or running automations.
- CLI (`cli/automation.rs`, U9): read ops (`list`/`show`/`runs`) read the store
  file directly (work outside a pane); mutating ops (`create`/`update`/`pause`/
  `resume`/`run`/`delete`) post over the hook socket (token-validated,
  origin-stamped, R22-gated). `create --verdict-gated` and `update
  --verdict-gated`/`--no-verdict-gated` (fly-dag-primitives G1) opt an
  agent-mode automation into non-monitor verdict parsing — same flag shape as
  `--headless`/`--paned`: the pair is mutually exclusive, and `--verdict-gated`
  is rejected with `--script` (a script has no prompt) and with `--monitor`
  (which already delivers a verdict and retires).
- **Update** (`docs/plans/2026-08-08-001-feat-automation-update-plan.md`, its
  own U1–U6/KTD1–KTD7): `fly automation update <id> [flags]` patches a stored
  record in place — name, schedule, prompt/model/effort/disposition, script
  content/interpreter/timeout — keeping the id, run history, origin and any
  dependency edge that the old delete + recreate dance destroyed.
  **Patch semantics** (KTD1): an absent field is unchanged, and clearing a pin
  rides an additive closed-set `clear: Vec<String>` on `AutomationRequest`
  (`model`, `effort`, `disposition`, `retryOnInterrupt`, `after`,
  `verdictGated`; an unknown member is refused, never ignored) — which is also
  the only way to express "retry off" or "verdict gating off", since both of
  those wire bools are skip-if-false.
  `AutomationManager::update` runs its gates **inside** the store mutation
  (the `resume` retirement-gate shape): unknown id, retired, monitor, mode
  mismatch. **The exclusions are the design** (KTD2), each with its own
  error: mode-kind switch (agent ↔ script), any update to a monitor (pickup
  pointers can't be re-captured without a registering pane) or a retired
  record, `cwd`, and **setting/re-pointing `after`** — only `--no-after`
  clears, so the dependencies plan's KTD6 cycle argument survives. A
  cron/timezone change recomputes `next_run_at` from now (not-before floor
  riding along) **only when the automation is live** — paused stays paused and
  picks the change up on resume; update never flips the enabled bit (KTD4).
  In-flight runs are never killed or re-parameterized (KTD3): a claimed row
  already carries its resolved model/effort/headless, and new script content
  is written to a **fresh** file whose pointer is swapped under the lock
  (KTD5 — a running interpreter holds the old one; the orphan is dropped
  after the lock). The untrusted socket payload is re-validated exactly like
  the create arm (KTD6), notably a `timeout_ms` over `TIMEOUT_MAX_MS` is
  **refused, never clamped**.
- Dashboard panel (U10): `lib/automations.ts` is the pure view-model
  (`automationsToRows` — sort next-run asc / paused last, mirroring the CLI —
  plus `humanSchedule`/`relativeTime`); `HomeView.svelte` renders it below the
  agent list, with the R6 store-health warning row. Interactive controls: the
  retired-fail monitor pickup (R16) and a per-row delete ✕ — routed through
  App's shared destructive-confirm into the `delete_automation` command, the
  webview counterpart of the CLI's `automation/delete` (same R23 teardown).

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
recompute) + a sparse recurring cron. **Checks dispatch headless**
(`docs/plans/2026-07-11-003-feat-headless-monitor-checks-plan.md`): a
backend-owned `claude -p --output-format stream-json` child
(`automations/headless.rs` — clean env via the shared pane strip list minus
the fly socket vars, tolerant init/result-only stream parse, monotonic
deadline, SIGTERM-first kill with a /proc descendant-snapshot sweep), NOT a
pane — no tab appears, `automation://agent-run` never fires, and a running
check is visible only as its dashboard automation row. The run output is the
stream's `result` event text (no transcript-capture race; the check's
`session_id` is stamped on the row and rides the FAIL bundle), routed through
`redact::clean_captured` and the manager's one shared verdict-close tail
(`close_headless_run` — the same retire/escalation/run-closed mutation the
pane path uses); anything surprising in the stream degrades to an
infra-unreadable Failed close, never a fabricated verdict. The check text is
parsed for one fenced ` ```verdict ` block (`automations/verdict.rs` — the
contract text is `VERDICT_BLOCK_SPEC`, quoted verbatim by
`skills/fly-monitor-handoff/SKILL.md`, edited only together; abstain-on-surprise,
so no block = "not done" = silent). A parsed PASS/FAIL verdict **retires** the
monitor in the same store mutation that closes the row (`retiredAt` set,
`next_run_at` cleared; claims/manual runs refused thereafter); FAIL also writes a durable
bundle file under `<data root>/monitor-bundles/` (outside the run-output tail
cap; evidence itself is tail-capped at 256 KiB at write time) and every verdict
rings via the existing Alert path. A **DECLINED** verdict never retires a
monitor: the monitor close path filters it out (fly-dag-primitives G1 KTD6) and
a stray one falls through as an ordinary not-done check — a monitor is a
done/not-done instrument, and its prompt contract lists only PASS/FAIL.
Three consecutive *unreadable* checks
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
  A record's `cwd` is the hook's **live** cwd, which drifts when the agent `cd`s
  away from its launch dir — but `claude --resume <id>` searches only the
  *launch* dir's project folder, so restore relocates it:
  `transcript.rs::resolve_resume_spawn_cwd` (a Tauri command, probed in parallel
  per precise leaf) verifies the recorded cwd's project folder actually holds
  the transcript, else scans the projects root for the file and recovers the
  launch cwd from the transcript's own `cwd` entries (the folder name can't be
  decoded — `-` is ambiguous — but exactly the launch cwd round-trips to it). A
  null or failed probe keeps the recorded cwd, so it is never worse than before.
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
  dashboard, settings menu, destructive-confirm — the latter also guards quit
  while agents are mid-work, via `home.ts::busyAgentCount`). All per-pane
  status polling (cwd, resume argv/session capture, activity, task count) is
  **one batched async `panes_status` invoke per 1.5 s worker tick**
  (`refreshPanes`, poll-batching plan 2026-08-08-003 — one `/proc` snapshot
  per call, ~5 s TTL on the session-id dir scan; keystrokes ride an async
  `pty_write` whose per-pane order `lib/write-chain.ts` pins). Don't add
  per-pane repeating invokes — extend `PaneStatus` instead.
- `lib/layout.ts` — **pure split-tree model**. Leaves render flat and keyed, so
  splitting/resizing never unmounts a pane (which would respawn its agent). Leaf
  keys are stable and also key the scrollback files — preserve this invariant.
  `App.svelte` renders every pane across **all** workspaces/tabs (hiding inactive
  ones) so switching never unmounts/respawns an agent — same invariant.
- `lib/pane-maps.ts` — the pure, vitest-tested half of the close-during-spawn
  fix (audit-remediation U9/KTD9), sitting beside `layout.ts` because it guards
  the same invariant from the other side: `resolveSpawnRace` (a pane that
  arrives after its component's cleanup ran has no owner — close it, never
  announce it) and `prunePaneIdMaps` (trim both pane-id maps to the live leaf
  set on every close path, dropping a stale reverse entry a reused pane id
  would otherwise resurrect).
- `lib/workspaces.ts` — **pure workspace/tab model** (mirrors `layout.ts`): a
  workspace is a named collection of tabs; helpers (`tabDisplayTitle`,
  `closeTabIn`, `deleteWorkspaceFrom`, `flattenRaised`) take id factories so
  they're tested without an app.
- `lib/keymap.ts` — the leader-key model (tmux-style: default Ctrl-A, then a
  command key; everything else passes through to the PTY). `BINDINGS` is the
  single source of truth shared by `dispatch()`, the hotkey menu, and the
  command palette, so they cannot drift — including `leader o`/`leader O`,
  which rotate focus forward/back through the active tab's panes in
  `layout.ts`'s leaf order.
- `lib/Terminal.svelte` — embeddable xterm leaf; subscribes to `pane://attention`.
  Terminal font size comes from config (`config.fontSize`, default 15).
  Renders through WebGL while its pane is in the active tab, disposed on hide
  (the KTD6 eviction, built as perf-audit T4 — `lib/renderer.ts` holds the
  pure attach rule; `renderer: "dom"` in config is the escape hatch).
- `lib/Sidebar.svelte` — collapsible cmux-style workspace tree with a
  **reason-typed attention dot** (`workspaces.ts::attentionKind` /
  `rollupAttentionKind`: amber = an agent is blocked on you, blue = one
  finished; input beats done, and anything non-`finished` folds to input so an
  unknown ask never reads as a calm completion — the rollup runs per tab over
  its leaves and per workspace over its tabs, so a collapsed workspace still
  shows its most urgent raised agent) over workspaces ▸
  named tabs; `lib/ControlBar.svelte` — slim top bar (sidebar toggle +
  breadcrumb + pane controls).
- `lib/transport.ts` — **the frontend's one transport seam** (Electron-shell
  migration U5): invoke/listen/pane-output-sink/window-close over either
  shell — Tauri (`@tauri-apps/api`) or the Electron preload bridge
  (`window.fly`), detected at runtime. `ipc.ts`, `lib/{config,serialize}.ts`,
  `main.ts`, `Terminal.svelte`, and `App.svelte` all route through it; no
  other file may import `@tauri-apps/api` directly. Bridge invokes JSON
  round-trip their args (Svelte 5 `$state` proxies fail Electron's
  structured clone; Tauri always JSON-serialized, so this preserves wire
  semantics exactly). `adoptLivePaneWithSink` is the re-attach half of
  renderer-crash recovery (Electron only — binds the sink to the existing
  pane id, discarding pre-bind frames: capture-then-subscribe, loss over
  duplication); `Terminal.svelte` tries it before every non-automation,
  non-ephemeral spawn. The Electron shell itself lives in `electron/`
  (main + preload + JS frame codec, edited with `docs/core-protocol.md`;
  `recovery.js` + `crashed.html` are the renderer-crash recovery — see
  `electron/README.md` "Renderer crash recovery").
- `lib/{config,serialize}.ts` (`serialize.migrateSession` upgrades old sessions
  into the workspace shape), `lib/HotkeyMenu.svelte` (passive cheat-sheet).
- `lib/SettingsMenu.svelte` — focus-taking toggle-settings modal (`leader ,`,
  the ⚙ control-bar button, or the palette). A dumb view: App owns the values
  (seeded from and persisted to config) and restores terminal focus on close.
- `lib/feed.ts` — the frontend half of `feed/`: pure wire-contract mirror of
  `feed/wire.rs` (`AgentEntry`/`AutomationEntry`/`VerdictEntry`) plus
  `buildFeedPayload`, which flattens the dashboard's grouped model into the
  pushed roster — reusing the dashboard's own status values so the feed can
  never drift from what fly displays.
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
  pure; frontend has vitest for layout/keymap; `pnpm test:unit` also runs the
  `electron/protocol.test.js` codec tests, and `src-tauri/tests/backend_build.rs`
  smoke-builds the full shared backend).
- Commits: conventional, with a `Co-Authored-By: Claude` trailer.
- Repo-root oddities an agent may trip over: `spikes/electron-probe/` is the
  retired measurement rig behind the Electron decision (kept as the record —
  never build on it; the real shell is `electron/`); `packaging/` holds only
  the one-shot icon toolchain (see its README — real packaging lives in
  `electron/package.json` + `src-tauri/tauri.conf.json`); the archived
  automations work-queue scratchpad lives at
  `docs/notes/2026-07-02-automations-work-queue-archive.md` (historical only).
- Versioning: keep `package.json`, `src-tauri/Cargo.toml`,
  `src-tauri/tauri.conf.json`, and `electron/package.json` on the SAME
  version — the Electron deb ships the Rust binary, and `fly --version` /
  `core/ping` report the crate version while dpkg reports the deb's.
