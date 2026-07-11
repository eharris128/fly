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

All plans below are **implemented in the current tree** (file paths are live);
a plan header's own `status:` field may lag behind the code.

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
| `2026-07-06-001-feat-feed-pending-question` | Pending agent questions on the feed: transcript backward-walk pending scan (choice/permission, delegating-tool abstention), `questionPendingAt` frame marker + gated `question` object on `/output` (scrub-then-truncate), Notification settle bump, durable 0600 config, guarded answer path (`mode:"keys"` + mandatory `ifAskedAt` + answered latch; permission answers config-gated, default off) | `session/transcript.rs`, `feed/{io,server,wire,mod}.rs`, `config/{mod,schema}.rs`, `lib.rs`, `lib/feed.ts` |
| `2026-07-09-001-feat-feed-conversation-tail` | Recent conversation tail (`turns`) on `GET /agents/{key}/output`: bounded transcript turn scan (reply-predicate parity), oldest→newest ending at the current reply (`at == repliedAt`), ≤12 turns × ≤2048 chars scrub-then-truncate, key omitted when no servable history | `session/transcript.rs`, `feed/{io,server,wire}.rs`, `lib/feed.ts` |
| `2026-07-10-001-feat-feed-question-screen-fallback` | Screen-derived pending-question fallback for Claude Code ≥ 2.1.206 (the transcript no longer flushes the ask at ask time): per-pane 64 KiB output tail ring, on-demand `vte` grid replay + shape-strict picker matcher (abstain-on-surprise, digits as rendered), corroboration chain (originally roster reason + `~/.claude/sessions` waiting; rebuilt three-valued around the sessions file alone by fix-feed-question-detection-gaps — see `docs/notes/2026-07-11-fix-feed-question-detection-gaps.md`), two-tier degrade (`questionPendingAt` survives body abstention on the corroborated leg), raise-stamp `askedAt` + `source:"screen"` provenance, widened permission opt-in | `pty/{pane,mod}.rs`, `feed/{screen,fallback,pending,io,server,wire,mod}.rs`, `session/livestate.rs`, `lib.rs`, `lib/feed.ts`, `tests/fixtures/screen/` |
| `2026-07-10-002-feat-monitor-handoff` | **Monitors** — parked experiments become retiring automations: not-before floor, fenced PASS/FAIL verdict block parsed at run close (retire-on-fire), durable failure bundle + pickup pointers captured at registration (refuse-or-store), broken-monitor escalation (3 unreadable checks), `create --monitor --not-before`, parent-tab auto-close, dashboard monitor states + one-action failed-monitor pickup, install-by-copy skill | `automations/{model,schedule,verdict,mod}.rs`, `session/handoff.rs`, `cli/automation.rs`, `lib.rs`, `lib/{automations,automation-panes,handoff}.ts`, `HomeView.svelte`, `App.svelte`, `skills/fly-monitor-handoff/` |
| `2026-07-11-001-feat-feed-other-answer` | Remote free-text ("Other") answers to a pending picker: `QuestionSpec.otherKey` (transcript body = source option count + 1; screen body = the rendered "Type something." row's own digit), `mode:"other"` on `POST /agents/{key}/input` (guarded like keys, sentence cap, refuse-without-digit), fly-owned digit → text → Enter three-chunk PTY choreography — no ESC anywhere (a bracketed paste's leading ESC cancels an unfocused picker; contract probed live on 2.1.206) | `feed/{wire,io,fallback,server}.rs`, `lib.rs`, `lib/feed.ts` |
| `2026-07-11-002-feat-hook-ask-channel` | Event-first pending-question detection + hook-answered permissions (stages 1–2 of the supervisor assessment; `PermissionRequest` contract live-verified on 2.1.207): `fly hooks setup` installs `PermissionRequest` (matcher `*`) → `fly notify --permission-request` forwards a bounded typed ask over the socket as a **held**, newline-framed `ask/hold` request (connection lifetime = ask lifetime; Claude kills the hook on local resolution → drop clears), `feed/ask.rs::AskRegistry` (leaf-keyed, last-write-wins, capped), hook leg ahead of transcript/screen in the resolver (`source:"hook"`, no reason gate — the held connection is the corroboration), `mode:"decision"` answers a hook-sourced permission ask through the hook's own response channel (opt-in-gated; the picker keeps keys/other — an allow can't skip it) | `hooks/{protocol,server}.rs`, `feed/{ask,fallback,io,server}.rs`, `cli/{notify,hooks}.rs`, `session/transcript.rs`, `lib.rs`, `lifecycle.rs`, `lib/feed.ts`, `tests/hook_ask.rs` |
| `2026-07-11-003-feat-headless-monitor-checks` | Monitor checks dispatch as backend-owned `claude -p --output-format stream-json` children instead of panes (empirical stream contract pinned on 2.1.207): tolerant init/result-only NDJSON parse + pure infra-vs-readable outcome classification (abstain-to-infra), clean env by inherit-minus-strip, monotonic deadline, SIGTERM-first kill with /proc descendant-snapshot sweep, `RunRow.headless`/`sessionId` + sweep exemptions + suspend-proof slack backstop, one shared verdict-close tail for pane and headless paths, `redact::clean_captured` extraction, FAIL-bundle "Check session" block; monitors bypass the frontend-ready claim gate (no event to drop) | `automations/{headless,model,mod,redact,verdict}.rs`, `pty/pane.rs`, `lib.rs`, `lifecycle.rs`, `tests/headless_runner.rs`, `tests/fixtures/headless/` |

## Brainstorms

`docs/brainstorms/` holds the requirements captured before a plan was written —
the upstream `origin:` a plan's header points at (six plans have one, from
dashboard-home-base through monitor-handoff). Start from the plan; drop to the
brainstorm for the "why".

## Other doc directories

- `docs/residual-review-findings/` — per-branch remainders from multi-agent
  code reviews: the deliberately-deferred findings (design calls a human still
  owns) plus suppressed lower-confidence risks worth keeping visible. Check
  here before re-reviewing or extending a feature — a "new" issue may already
  be recorded and triaged.
- `docs/notes/` — one-off evaluations and research notes that are neither a
  plan nor a brainstorm (e.g. the conductor-oss evaluation, the
  fix-feed-question-detection-gaps root-cause post-mortem).
