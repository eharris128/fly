# Design plans index

`docs/plans/` is the full design for **fly**. The code is cross-referenced back
to these docs by ID in doc comments (see `CLAUDE.md` → "Conventions"). This index
maps each plan to what it added and the primary code that implements it, so you
can go from a `KTD`/`R`/`U` reference in the source to the doc that defines it.

## How the IDs work (read this first)

IDs are **scoped per plan** — every plan restarts its own `KTD<n>` (key technical
decision), `R<n>` (requirement), and `U<n>` (unit) numbering. So `KTD7` in the
foundation plan is a *different* decision from `KTD7` in the automations plan.
**Always resolve an ID against the plan the file belongs to** (use the "Primary
code" column below, or the plan reference in the file's own doc comment). Some
plans continue a running range from an earlier related plan (e.g. the
notification-parity plan starts at `U16`); the per-plan doc is always the
authority.

Plans whose upstream requirements were captured in a brainstorm session link to
`docs/brainstorms/` via an `origin:` header.

All plans below are **implemented in the current tree** (file paths are live)
unless the row is marked **Planned**; a plan header's own `status:` field may
lag behind the code.

## Plans (chronological)

| Plan (`docs/plans/…`) | What it added | Primary code |
|---|---|---|
| `2026-06-16-001-feat-fly-agent-terminal` | **Foundation** — PTY panes, tabs/splits, the attention pipeline, the authenticated hook socket, the `fly` CLI | `pty/`, `stream/`, `state/{lifecycle,attention,policy,manager}`, `hooks/`, `notify/`, `cli/{notify,hooks}`, `config/`, `session/mod.rs`, `App.svelte`, `lib/{layout,keymap,config,serialize}.ts`, `Terminal.svelte` |
| `2026-06-18-001-feat-hotkey-menu` | Cheat-sheet overlay + close-tab chord | `lib/HotkeyMenu.svelte`, `lib/keymap.ts` |
| `2026-06-19-001-feat-notification-parity-suppression` | Notification panel, badges, and the suppression policy (`U16`+) | `state/policy.rs`, `notify/`, `lib/notifications.ts`, `lib/NotificationPanel.svelte` |
| `2026-06-22-001-feat-tab-workspace-qol` | Workspaces, sidebar tree, control bar, tab rename | `lib/workspaces.ts`, `lib/Sidebar.svelte`, `lib/ControlBar.svelte`, `lib/layout.ts` |
| `2026-06-22-002-feat-agent-dashboard-home` | Agent dashboard (`leader d`): output-activity signal + `/usage` gauges | `state/activity.rs`, `usage/`, `lib/home.ts`, `lib/HomeView.svelte` |
| `2026-06-23-001-feat-resume-agents` | Durable per-leaf resume store + clean-exit marker + replay argv | `session/resume.rs`, `lib/resume.ts` |
| `2026-06-23-002-feat-dashboard-running-state` | `running · N tasks` status for busy-but-quiet agents | `lib/home.ts`, `cwd/` (`/proc` task probe) |
| `2026-06-23-003-fix-resume-session-selection` | Transcript-derived session-id capture (version-skew-proof) | `session/transcript.rs` |
| `2026-06-30-001-feat-dashboard-home-base` | Dashboard "home base" + the attention-triage nudge | `lib/HomeView.svelte`, `lib/home.ts`, `lib/nudge.ts`, `lib/NudgeOverlay.svelte` |
| `2026-07-01-001-feat-reason-typed-attention-triage` | `Reason`-typed attention (`Reason::Alert` end-to-end) + triage badge | `state/attention.rs`, `hooks/`, `lib/notifications.ts` |
| `2026-07-01-002-feat-automations` | Cron-scheduled agent/script runs (`U1`–`U12`) | `automations/`, `cli/automation.rs`, `lib/automations.ts`, `lib/automation-panes.ts` |
| `2026-07-02-001-feat-session-handoff` | Fresh agent handed a stale pane's previous session (`leader f`/`F`) | `session/handoff.rs`, `lib/handoff.ts` |
| `2026-07-03-001-fix-session-pane-attribution` | Trust-ranked (`Poll < Hook < Pick`) SessionStart capture, poll abstention on same-cwd ambiguity, session pick-list, reset/re-pick (`leader g`) | `session/{resume,transcript,handoff}.rs`, `cli/{hooks,notify}.rs`, `hooks/protocol.rs`, `lib/session-picker.ts`, `lib/SessionPicker.svelte` |
| `2026-07-03-002-feat-automations-workspace-and-model` | Dedicated **Automations** workspace (durable `role` marker, auto-provision, auto-close on success) + per-automation `--model`/`--effort` with a shared default + `sonnet` fallback; agent final message captured from the transcript | `automations/{model,mod}.rs`, `config/schema.rs`, `cli/automation.rs`, `session/transcript.rs`, `lib/{workspaces,serialize,automation-panes,automations}.ts`, `App.svelte` |
| `2026-07-04-001-feat-agent-state-local-feed` | Read-only loopback SSE feed of agent/automation state (bearer-token auth), always-on frontend publisher | `feed/`, `lib/feed.ts`, `App.svelte`, `lib.rs` |
| `2026-07-05-001-feat-automations-interrupt-resilience` | Interrupted automation runs (app crash/restart) surface through the alert pipeline + emit `automation://changed`; opt-in `retry_on_interrupt` re-runs once as `Trigger::Retry` (retry-once crash-loop guard; agent retries honor frontend-ready) | `automations/{model,mod}.rs`, `cli/automation.rs`, `lib.rs`, `ipc.ts`, `lib/automations.ts`, `lib/HomeView.svelte` |
| `2026-07-05-002-feat-feed-agent-reply-io` | Feed endpoints for per-agent reply read (`GET /agents/{key}/output`) + input write (`POST /agents/{key}/input`, bracketed-paste + Enter, attention-clearing) + emit-time `lastReplyAt` on `/feed` frames with a post-Stop settle bump | `feed/{io,server,wire,mod}.rs`, `session/transcript.rs`, `pty/mod.rs`, `lib.rs`, `lib/feed.ts` |
| `2026-07-06-001-feat-feed-pending-question` | Pending agent questions on the feed: transcript backward-walk pending scan (choice/permission, delegating-tool abstention), `questionPendingAt` frame marker + gated `question` object on `/output` (scrub-then-truncate), Notification settle bump, durable 0600 config, guarded answer path (`mode:"keys"` + mandatory `ifAskedAt` + answered latch; permission answers config-gated, default off); transcript scan since demoted behind the held hook ask (`2026-07-11-002`) with the screen fallback behind it (`2026-07-10-001`) — see the plan's addendum | `session/transcript.rs`, `feed/{io,server,wire,mod}.rs`, `config/{mod,schema}.rs`, `lib.rs`, `lib/feed.ts` |
| `2026-07-09-001-feat-feed-conversation-tail` | Recent conversation tail (`turns`) on `GET /agents/{key}/output`: bounded transcript turn scan (reply-predicate parity), oldest→newest ending at the current reply (`at == repliedAt`), ≤12 turns × ≤2048 chars scrub-then-truncate, key omitted when no servable history | `session/transcript.rs`, `feed/{io,server,wire}.rs`, `lib/feed.ts` |
| `2026-07-10-001-feat-feed-question-screen-fallback` | Screen-derived pending-question fallback for Claude Code ≥ 2.1.206 (the transcript no longer flushes the ask at ask time): per-pane 64 KiB output tail ring, on-demand `vte` grid replay + shape-strict picker matcher (abstain-on-surprise, digits as rendered), corroboration chain (originally roster reason + `~/.claude/sessions` waiting; rebuilt three-valued around the sessions file alone by fix-feed-question-detection-gaps — see `docs/notes/2026-07-11-fix-feed-question-detection-gaps.md`), two-tier degrade (`questionPendingAt` survives body abstention on the corroborated leg), raise-stamp `askedAt` + `source:"screen"` provenance, widened permission opt-in | `pty/{pane,mod}.rs`, `feed/{screen,fallback,pending,io,server,wire,mod}.rs`, `session/livestate.rs`, `lib.rs`, `lib/feed.ts`, `tests/fixtures/screen/` |
| `2026-07-10-002-feat-monitor-handoff` | **Monitors** — parked experiments become retiring automations: not-before floor, fenced PASS/FAIL verdict block parsed at run close (retire-on-fire), durable failure bundle + pickup pointers captured at registration (refuse-or-store), broken-monitor escalation (3 unreadable checks), `create --monitor --not-before`, parent-tab auto-close, dashboard monitor states + one-action failed-monitor pickup, install-by-copy skill | `automations/{model,schedule,verdict,mod}.rs`, `session/handoff.rs`, `cli/automation.rs`, `lib.rs`, `lib/{automations,automation-panes,handoff}.ts`, `HomeView.svelte`, `App.svelte`, `skills/fly-monitor-handoff/` |
| `2026-07-11-001-feat-feed-other-answer` | Remote free-text ("Other") answers to a pending picker: `QuestionSpec.otherKey` (transcript body = source option count + 1; screen body = the rendered "Type something." row's own digit), `mode:"other"` on `POST /agents/{key}/input` (guarded like keys, sentence cap, refuse-without-digit), fly-owned digit → text → Enter three-chunk PTY choreography — no ESC anywhere (a bracketed paste's leading ESC cancels an unfocused picker; contract probed live on 2.1.206) | `feed/{wire,io,fallback,server}.rs`, `lib.rs`, `lib/feed.ts` |
| `2026-07-11-002-feat-hook-ask-channel` | Event-first pending-question detection + hook-answered permissions (stages 1–2 of the supervisor assessment; `PermissionRequest` contract live-verified on 2.1.207): `fly hooks setup` installs `PermissionRequest` (matcher `*`) → `fly notify --permission-request` forwards a bounded typed ask over the socket as a **held**, newline-framed `ask/hold` request (connection lifetime = ask lifetime; Claude kills the hook on local resolution → drop clears), `feed/ask.rs::AskRegistry` (leaf-keyed, last-write-wins, capped), hook leg ahead of transcript/screen in the resolver (`source:"hook"`, no reason gate — the held connection is the corroboration), `mode:"decision"` answers a hook-sourced permission ask through the hook's own response channel (opt-in-gated; the picker keeps keys/other — an allow can't skip it) | `hooks/{protocol,server}.rs`, `feed/{ask,fallback,io,server}.rs`, `cli/{notify,hooks}.rs`, `session/transcript.rs`, `lib.rs`, `lifecycle.rs`, `lib/feed.ts`, `tests/hook_ask.rs` |
| `2026-07-11-003-feat-headless-monitor-checks` | Monitor checks dispatch as backend-owned `claude -p --output-format stream-json` children instead of panes (empirical stream contract pinned on 2.1.207): tolerant init/result-only NDJSON parse + pure infra-vs-readable outcome classification (abstain-to-infra), clean env by inherit-minus-strip, monotonic deadline, SIGTERM-first kill with /proc descendant-snapshot sweep, `RunRow.headless`/`sessionId` + sweep exemptions + suspend-proof slack backstop, one shared verdict-close tail for pane and headless paths, `redact::clean_captured` extraction, FAIL-bundle "Check session" block; monitors bypass the frontend-ready claim gate (no event to drop) | `automations/{headless,model,mod,redact,verdict}.rs`, `pty/pane.rs`, `lib.rs`, `lifecycle.rs`, `tests/headless_runner.rs`, `tests/fixtures/headless/` |
| `2026-07-16-001-feat-automations-usage-limit-deferral` | Usage gate for scheduled agent-mode dispatch: a due occurrence at a confidently-exhausted plan window (session/weekly, per the dashboard's `/api/oauth/usage` shape) records a pre-claim `Skipped("usage limit")` row and defers `next_run_at` through the existing `advance_from` floor to the window's `resets_at` (composed with a monitor's not-before via max); gate verdict resolved before the store lock, consulted only when an agent-mode claim is possible; fail-open on every uncertainty (incl. active overage); scripts/manual never gated, unattended retries skip-close (retry-once); offset-aware RFC3339 parse; `usageGate` config knob (default on); **U6 deferred** — the at-limit close-honesty pin (pane `Stop ⇒ Succeeded` misclassification) awaits a real at-limit window | `usage/gate.rs`, `automations/mod.rs`, `session/transcript.rs`, `config/schema.rs`, `lib.rs` |
| `2026-07-17-001-fix-audit-remediation` | Remediation of the 2026-07-17 full-codebase audit: feed reply secret-scrub parity (closing `resolve_io`'s documented deferral), permission-opt-in coverage for unguarded submits (409 `askPending`), `release-dev` overflow-checks, plus the low-severity tail (store poison recovery, fsync-before-rename, connection caps, socket-dir 0700, grid-replay clamp, spawn-race guard + id-map pruning, leader double-tap literal, resume `--session-id` strip, CSP, comment hygiene) | `feed/{io,server}.rs`, `automations/{store,alerts,script}.rs`, `hooks/server.rs`, `session/resume.rs`, `feed/screen.rs`, `lib.rs`, `Cargo.toml`, `tauri.conf.json`, `lib/{keymap,resume,pane-maps}.ts`, `Terminal.svelte`, `App.svelte` |
| `2026-07-24-001-feat-phone-screenshot-drop` | **Phone screenshot drop** — a tailnet-served upload page that delivers a screenshot plus caption into a live agent pane: `POST /drop` (raw body, caption in the query, format sniffed from the bytes, streaming temp-then-rename with unlink-on-refusal) and an inert unauthenticated `GET /` page shell (`include_str!`, no Vite dependency); `paneId` + `publishedAt` added to the roster so delivery can refuse a replaced session and diagnose a frozen webview; two check-then-act delivery guards (pane identity + a `/proc` foreground re-probe before the Enter, so an exited `claude` can't have the caption executed as a shell command); the question gate **widened to any pending question for this route only**, since `paste_payload`'s leading ESC silently cancels an unfocused picker; every refusal drains the body first (`tiny_http` would otherwise size a drop-time allocation from the client-declared `Content-Length`); reachability comes from `tailscale serve`, fly adds no bind | `feed/{drop.rs,drop-page.html,server.rs,wire.rs,mod.rs}`, `config/schema.rs`, `lib.rs`, `lib/{feed,config}.ts`, `App.svelte` |
| `2026-07-31-001-feat-headless-agent-automations` | Regular agent automations dispatch closed-loop headless (`claude -p` via the monitor runner) by default: `Mode::Agent.headless` + the `automation_defaults.headless` knob (ships **true** — existing automations flip on upgrade unless `--paned`) resolved at claim via `Mode::resolved_headless` and stamped on the row, routing forks on the claimed row's marker in `CompositeDispatcher`; headless claims/retries carve out of the frontend-ready deferral (usage gate still consulted); a Failed headless close rings the alert path (the kept-failed-tab replacement), Succeeded stays silent, non-monitors never verdict-parse; `create --headless`/`--paned` (CLI + wire validated), `show` dispatch line, `runs` sessionId + `-v` transcript path; dashboard `running · Xm` elapsed read + `headlessDefault` on the DTO; feed `AutomationEntry.headless` (additive) | `automations/{model,mod,headless}.rs`, `config/schema.rs`, `cli/automation.rs`, `lib.rs`, `feed/wire.rs`, `lib/{automations,feed}.ts`, `HomeView.svelte`, `App.svelte`, `ipc.ts` |
| `2026-08-07-001-feat-agent-peer-messaging` | Agent peer addressing: `fly agents` (socket-served roster read with explicit staleness — deliberately *not* a file read; the roster has no honest at-rest form) + `fly send` (guarded, sanitized, provenance-framed delivery into an opted-in pane) as `peer/*` ops on the hook socket; token-resolved sender identity (no "from" on the wire, no `reason` field ever — the skew rule), default-closed **session-scoped human-only** receive opt-in (dashboard "peers" toggle → roster push, no socket/CLI/config writer), wide any-question gate shared with the drop route, per-sender + global rate buckets (hop counters rejected as unenforceable), app-wide delivery mutex against spliced pastes | `hooks/{protocol,server}.rs`, `cli/{peer,mod}.rs`, `peer/{mod,compose,rate,list}.rs`, `feed/{wire,mod,server}.rs`, `lib.rs`, `lib/feed.ts`, `HomeView.svelte`, `App.svelte`, `tests/peer_send.rs` |
| `2026-08-07-002-feat-automation-dependencies` | **Automation dependencies** — `fly automation create --after <id> [--within <dur>]`: a dependent's cron occurrence fires only against a fresh, successful, not-yet-consumed upstream run (bounded wait, then an honest born-terminal `withheld` row naming why — never a green row over stale data); exactly-once consumption via `RunRow.upstreamRunId` stamped atomically with the claim; create-time chain-depth/cycle rejection; withholds ring the Automations alert path; additive feed/dashboard projection (`after`, `lastWithheldReason`, `waiting on upstream`) | `automations/{model,depend,mod}.rs`, `cli/automation.rs`, `lib.rs`, `feed/{wire,mod}.rs`, `lib/{automations,feed}.ts`, `HomeView.svelte` |
| `2026-08-08-001-feat-automation-update` | **`fly automation update`** (planned) — patch a stored automation in place (name/cron/timezone, agent prompt/model/effort/disposition, script content/interpreter/timeout, retry toggle) with patch semantics (absent = unchanged, closed-set `clear` list for explicit resets); refuses the load-bearing mutations: mode-kind switch, monitors, retired records, setting/changing `after` (clear-only, preserving the dependencies plan's KTD6 cycle argument), and `cwd` (confidentiality-guard interaction); in-flight runs untouched (claimed rows are self-stamped), cron recompute only-when-enabled, script content swapped write-new-then-swap | `automations/{mod,store}.rs`, `cli/automation.rs`, `lib.rs` |

The plan dir also holds one non-plan artifact:
`2026-07-03-002-automations-workspace-and-model-LIVE-CHECKLIST.md`, the
live-validation checklist that accompanied the workspace-and-model plan.

Not every merge has a plan. Small QoL features and fixes are documented in
`CLAUDE.md` only: the settings-menu overlay (`leader ,`); quit-confirm while
agents are mid-work; `leader o`/`O` pane-focus rotation (`e5e4781`); the
sidebar's reason-typed attention dot, amber for input vs blue for done
(`cdfa6b3`); the per-row automation delete on the dashboard (`97dd705`); and the
resume launch-dir relocation for a drifted session cwd (`b821ca4`). The
feed-monitor-enrichment follow-up (`monitor` / `retiredAt` /
`lastVerdict` on the feed's automation projection, commit `608bb46` — designed
in the *game* repo's Ambient Wall plan) is recorded in
`docs/notes/2026-07-16-feed-monitor-enrichment.md`.

## Brainstorms

`docs/brainstorms/` holds the requirements captured before a plan was written —
the upstream `origin:` a plan's header points at (seven plans have one, from
dashboard-home-base through phone-screenshot-drop). Start from the plan; drop to
the brainstorm for the "why".

## Other doc directories

- `docs/residual-review-findings/` — per-branch remainders from multi-agent
  code reviews: the deliberately-deferred findings (design calls a human still
  owns) plus suppressed lower-confidence risks worth keeping visible. Check
  here before re-reviewing or extending a feature — a "new" issue may already
  be recorded and triaged.
- `docs/notes/` — one-off evaluations and research notes that are neither a
  plan nor a brainstorm (e.g. the conductor-oss evaluation, the
  fix-feed-question-detection-gaps root-cause post-mortem, the
  feed-monitor-enrichment record). Two carry content you'd otherwise hunt for:
  **`2026-07-24-phone-drop-live-check.md` holds the phone drop's operator setup**
  (the `tailscale serve` mount, the token, the identity knob) under its
  "Operator setup (U8)" heading, and `2026-07-23-performance-audit-follow-ups.md`
  is the open task list from that audit. A third is load-bearing for anyone
  touching the ask channel: **`2026-08-07-peer-messaging-live-check.md` records
  a held-ask leak on Claude Code 2.1.224** — a locally-answered dialog no longer
  kills its `PermissionRequest` hook, so the held ask never clears and every
  question-gated surface (peer `send`, phone drop, the feed input route) refuses
  indefinitely. Open as of 2026-08-07, with candidate fixes.
