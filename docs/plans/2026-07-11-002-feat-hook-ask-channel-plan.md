<!--
Plan: hook-ask-channel
Status: implemented (see README.md index)
IDs (KTD/R/U) are scoped to THIS plan (repo convention: per-plan numbering).
Origin: stages 1–2 of the supervisor-architecture assessment (session 7f6f5187,
2026-07-11) grounded in the external research report
"Consuming Claude Code as a Data Source" (compass artifact); the report's own
Stage 1 ("replace state-guessing with structured hook events") and the
session's Stage 2 ("prefer hook-answering over keystroke injection").
-->

# feat: Hook-delivered pending questions + hook-answered permissions (`hook-ask-channel`)

## Summary

Everything hard about fly's pending-question detection lives at the bottom of
the stack: the transcript no longer flushes the ask at ask time (≥ 2.1.206),
the sessions-file gate is three-valued and absent for child sessions, and the
screen fallback VT-parses a rendered picker with abstain-on-surprise
discipline. All of that machinery exists because fly had no *event* that says
"a dialog just went up, and here is exactly what it asks".

Claude Code has since shipped that event. The **`PermissionRequest` hook**
(v2.0.45+) fires when a permission dialog *would* show — **including for
`AskUserQuestion`, including under `bypassPermissions`** — carrying the tool
name and full `tool_input` (for AskUserQuestion: the entire `questions`
array), ~6 seconds *before* the `Notification` hook fly already consumes. And
it can **answer**: a hook that emits a `decision` dismisses the dialog and
resolves the ask, which is exactly the structured answer path issue #38299
shows keystroke injection can never reliably be.

This plan wires that event into fly's existing authenticated per-pane socket
as a **held request**: `fly notify --permission-request` forwards the ask and
then *stays connected* until the ask resolves. The feed serves the held ask as
its **primary** pending-question source (`source:"hook"`), ahead of the
transcript walk and the screen fallback, which both remain as fallbacks for
older Claude versions and hook-less panes. A new guarded input mode
(`mode:"decision"`) answers a held *permission* ask through the hook's own
response channel — no PTY bytes, no picker digits. The AskUserQuestion picker
keeps the existing `keys`/`other` injection path: an `allow` decision cannot
skip a `requiresUserInteraction` tool (verified), so the picker is answered on
screen or not at all.

The game/feed consumer contract is unchanged except for two strict additions:
`question.source` may now be `"hook"`, and `mode:"decision"` exists.

## The hook contract (probed live, 2026-07-11, Claude Code 2.1.207)

Driven against a real interactive `claude` in tmux with logging / sleeping /
decision-emitting `PermissionRequest` hooks (scratchpad `hookprobe`; full
findings in the `permissionrequest-hook-contract` project memory):

1. **Fires at ask time for every dialog.** A Bash permission and an
   AskUserQuestion picker both fired `PermissionRequest` the moment the dialog
   was due; the `Notification(permission_prompt)` hook followed ~6.0s later
   (the #58909 idle gate). Payload carries `session_id`, `transcript_path`,
   `cwd`, `prompt_id`, `permission_mode`, `tool_name`, `tool_input`, and
   (permission dialogs) `permission_suggestions`.
2. **Fires under `bypassPermissions` for AskUserQuestion** —
   `permission_mode:"bypassPermissions"` rode the payload. Automation panes
   (`--dangerously-skip-permissions`) are covered.
3. **The dialog renders ~0.4s after the hook starts, while the hook still
   runs.** A blocking hook does not delay or hold the TUI; the local user can
   always answer immediately.
4. **A decision emitted mid-dialog wins.** A hook that slept 6s and then
   emitted `deny` dismissed the visible dialog and resolved the turn
   ("Denied by PermissionRequest hook"). Schema (official docs, verified):
   `{"hookSpecificOutput":{"hookEventName":"PermissionRequest",
   "decision":{"behavior":"allow"|"deny","message":…,"updatedInput":…,
   "addPermissionRuleOnAllow":…,"updatedPermissions":…}}}`.
5. **A local answer while the hook runs kills the hook process.** The hook's
   post-kill output is never read; a killed `fly notify`'s socket connection
   drops. Connection lifetime therefore mirrors ask lifetime — including when
   the "local" answer is fly's own injected picker keys.
6. **`allow` cannot skip the AskUserQuestion picker** (requiresUserInteraction,
   v2.1.199+): the picker rendered despite an instant allow. Hook-answering is
   permission-kind only.
7. Default command-hook timeout is 600s (per-hook `"timeout"` in seconds); on
   timeout Claude kills the hook and the dialog is unaffected.

## Key Technical Decisions

- **KTD1 — the ask rides the existing authenticated socket as a *held*,
  newline-framed request.** No new listener, no new trust boundary:
  `fly notify --claude --permission-request` presents the same per-pane token
  and faces the same constant-time validation, peer-UID check, and lockout as
  every message. Unlike the fire-and-forget notify path (EOF-framed), the
  client terminates its request with `\n` and **keeps the connection open**;
  the server replies with an immediate ack line, then holds. The connection
  *is* the ask's live state: Claude kills the hook when the dialog resolves
  locally (probed contract #5), so a dropped connection means "resolved" with
  no polling and no timers. Resolution paths: answer written (remote decision),
  peer drop (local answer / hook timeout / claude exit), or fly shutdown
  (empty-line release).
- **KTD2 — the ask registry is leaf-keyed, last-write-wins, bounded.** One
  dialog per session at a time is Claude's own modality; a second ask arriving
  for the same leaf releases the previous held connection (stale by
  definition) and replaces it. A registry-wide cap (`MAX_HELD_ASKS`, 64)
  bounds held threads; at the cap a new ask is acked-and-released immediately
  (degrades to the existing detection chain, never blocks the hook 600s).
- **KTD3 — a held ask is the primary question source; the reason gate does
  not apply to it.** Resolution order becomes hook → transcript → livestate/
  screen (the two existing legs, byte-identical when no ask is held). A
  transcript-derived *permission* body needs the live attention reason as
  corroboration because a pending `tool_use` alone can just mean "executing";
  a held `PermissionRequest` connection cannot — the event fires only when a
  dialog is actually up, and the connection drops when it resolves. So a
  hook-sourced body (and its `questionPendingAt`) is exposed regardless of
  reason, closing the gap where a raise is instantly acknowledged on a
  visible pane. `source:"hook"` marks provenance; `askedAt` is fly's own
  receipt stamp (never a transcript stamp — same discipline as screen bodies,
  and the same 409-on-stale `ifAskedAt` consequence when a source handover
  moves the stamp).
- **KTD4 — the CLI extracts a bounded, typed subset client-side; an
  unparseable payload still registers a body-less ask.** `tool_input` is
  unbounded (a `Write` permission can carry a whole file), so the raw payload
  never crosses the socket. `fly notify` reads up to 1 MiB of stdin and
  forwards only: `tool_name`, `permission_mode`, `session_id`, `cwd`, the
  AskUserQuestion `questions` array (strings pre-capped, options/questions
  count-capped), and the known one-line request fields (`Bash.command`,
  `Edit/Write.file_path`), everything re-cleaned server-side (io::clean) at
  serve time as usual — the client cap is a transport bound, not the
  sanitization boundary. If stdin is over-cap or unparseable, the client
  sends the ask with no body fields: fly still learns *that* a dialog is up
  (tier-1 `questionPendingAt` + held-connection resolution tracking), serving
  a body-less permission ask. Degrade, never abstain.
- **KTD5 — hook-answering is permission-kind only, behind the existing
  opt-in, delivered on the held connection.** `mode:"decision"`
  (`{"mode":"decision","decision":"allow"|"deny","ifAskedAt":…}`) reuses the
  guarded-answer machinery wholesale: mandatory `ifAskedAt`, fresh gate
  re-read, the per-leaf answered latch, and `feed.allowPermissionAnswers`
  (default off) — a decision is definitionally a remote permission answer.
  It 409s unless the currently gated question is hook-sourced,
  permission-kind, stamp-matched, AND the held connection is still alive.
  Delivery writes the probed `hookSpecificOutput` decision JSON to the held
  connection — never PTY bytes. AskUserQuestion asks are never
  decision-answerable (contract #6); their picker keeps `keys`/`other`,
  whose digits a hook-sourced choice body carries as index+1 (the picker
  numbers authored options 1..N — same arithmetic, same `otherKey` rule as a
  transcript body, re-verified on 2.1.207).
- **KTD6 — the attention pipeline is untouched.** The Notification hook (6s
  later) still owns raising/ringing; `PermissionRequest` only feeds the feed
  (registry + `FeedState::bump` on register/clear, so SSE frames move without
  a roster change) and — like every session_id-bearing hook — upserts the
  pane's resume record at `Hook` rank on registration, which also gives the
  resolver its session attribution at ask time. Earlier-ring-on-ask is
  deliberately deferred (Residual).

## Requirements

- **R1** — `fly hooks setup` additionally installs `PermissionRequest`
  (matcher `"*"`) → `fly notify --claude --permission-request`; teardown
  removes it; both idempotent, user hooks untouched. On a Claude too old for
  the event the hook never fires and every existing surface behaves
  byte-identically (the fallback chain is unchanged downstream).
- **R2** — the security boundary does not weaken: token validation (constant
  time + lockout) and peer-UID precede any held work; the request line is
  bounded (`MAX_MESSAGE`) with a wall-clock bound on the request phase (a
  byte-trickling peer cannot hold a pre-validation thread); held connections
  are capped (KTD2); rejection stays silent.
- **R3** — register → the leaf's `questionPendingAt` (and gated `question`
  body) surfaces on the next frame, with a `FeedState::bump` fired on both
  register and clear so a frame *does* move; drop → cleared on the next
  resolve. No polling anywhere in the path.
- **R4** — a hook-sourced AskUserQuestion body is built through the same
  shaping/clean pipeline and caps as a transcript body (`question_body`),
  with identical `answerable` rules, digits = index+1, and `otherKey` =
  source count + 1; `asked_at` is the registry receipt stamp.
- **R5** — a hook-sourced permission body carries the tool name and the
  known-field request summary, is never `answerable`, and is exposed without
  reason corroboration (KTD3). A body-less ask (KTD4 degrade) still stamps
  tier-1 `questionPendingAt` and serves a minimal permission body.
- **R6** — `mode:"decision"`: 400 for a missing/unknown `decision` value or
  missing `ifAskedAt`; 404/409/403 follow the pinned input-route precedence
  (403 = `permissionAnswersDisabled`, same discriminator body); 409 when the
  gated question is not hook-sourced permission-kind, when `ifAskedAt`
  mismatches, when the latch already holds the stamp, or when the held
  connection is gone (a raced local answer). One decision per ask (latch +
  registry removal are atomic with the write).
- **R7** — a delivered decision writes exactly the probed schema (allow, or
  deny with a fixed provenance message), the registry entry clears, and pane
  attention clears the same way other input modes do. A local answer that
  races ahead simply drops the connection: the late `mode:"decision"` 409s.
- **R8** — version skew both ways: a NEW `fly notify --permission-request`
  against an OLD app (no ack within 2s — the old server silently ignores the
  unknown-op message) exits 0 with no output and the dialog proceeds
  normally; an OLD `fly notify` against a NEW app is byte-identical (notify
  path untouched, EOF framing still honored).
- **R9** — ordered shutdown releases every held ask (write-then-close) before
  the socket server stops: no hung hook processes, no zombies (lifecycle.rs
  ordering).
- **R10** — `submit`/`keys`/`other` behavior, the transcript leg, and both
  screen-fallback legs are byte-identical whenever no held ask exists for a
  leaf.

## Units

- **U1 — protocol** (`hooks/protocol.rs`): `op:"ask/hold"` on the envelope +
  the typed `AskPayload` (tool, permission_mode, session_id, cwd, questions
  Value, request summary — all `#[serde(default)]`), newline-framing note in
  the schema doc-comment, ack/decision response line shapes. Tests: op
  routing, back-compat both directions.
- **U2 — held server path** (`hooks/server.rs`): request read becomes
  newline-or-EOF bounded with a request-phase deadline (R2); an `ask/hold` op
  routes to the injected `AskHandler` seam (mirrors `RequestHandler`): ack
  line, register, then block on {answer channel, peer-drop probe, shutdown},
  respond/close. Tests ride the existing socket-test harness + a new
  `tests/hook_ask.rs` integration file.
- **U3 — registry** (`feed/ask.rs`): `AskRegistry` — leaf-keyed held asks
  (payload + receipt stamp + responder), register (last-write-wins, cap),
  clear-on-drop, `answer(leaf, behavior)`, snapshot getters for the resolver,
  `shutdown()`. Pure where possible; fully unit-tested.
- **U4 — CLI** (`cli/notify.rs`): `--permission-request` — 1 MiB stdin read,
  bounded extraction (KTD4), connect/send/ack-or-exit(2s)/hold/print-decision
  loop; never blocks without a deadline on the ack phase. Unit tests for
  extraction + caps.
- **U5 — hook install** (`cli/hooks.rs`): `PermissionRequest` in
  `CLAUDE_HOOK_EVENTS` with matcher support (the existing events install
  matcher-less; this one installs `"*"`). Setup/teardown/idempotency tests.
- **U6 — resolver hook leg** (`feed/fallback.rs` + `feed/io.rs`): an injected
  `AskFn` seam consulted FIRST in `resolve_io`; ask → `PendingInteraction` →
  the shared `question_body` shaping (R4/R5), `source:"hook"`. The
  AskUserQuestion `questions` Value reuses `session/transcript.rs`'s question
  parser (exposed, not duplicated). Fixture tests: primacy over transcript
  and screen, body shapes, clean-pipeline application, stamp discipline.
- **U7 — feed exposure + answer route** (`feed/server.rs`, `feed/wire.rs`):
  `gated_question` exempts `source:"hook"` from the permission-reason gate
  (KTD3); `mode:"decision"` parsing + guard chain + `InputAction::Decision`
  (R6); `screen_under_permission` widening explicitly does not apply to hook
  bodies (authoritative tool name, transcript parity). Precedence/latch/race
  tests.
- **U8 — wiring** (`lib.rs`, `lifecycle.rs`): registry construction (managed
  unconditionally, like `FeedState`), `AskHandler` closure (leaf resolution,
  resume upsert at `Hook` rank, register, bump), `AskFn` + decision delivery
  into the feed seams, shutdown ordering (R9).
- **U9 — integration test** (`tests/hook_ask.rs`): real socket round-trips —
  register→resolve, drop→clear, answer→decision-line, replacement, cap,
  no-ack fast-fail client behavior, shutdown release.
- **U10 — docs**: CLAUDE.md (feed + attention-pipeline sections),
  `hooks/CLAUDE.md` (held-connection invariants), `docs/plans/README.md` row,
  `src/lib/feed.ts` mirror comment for `source:"hook"` + `mode:"decision"`.

## Live verification checklist (in-app, post-merge)

The hook contract itself is live-verified (2.1.207 probe, see the contract
section); the socket round-trip is integration-tested end-to-end
(`tests/hook_ask.rs`), and the installed binary's degradation paths
(no env / no app: exit 0, no stdout, <50 ms) are smoke-tested. What still
wants one pass in the running app:

1. `fly hooks setup` (the dev-flavor binary), open a pane, run a `claude`
   that triggers a Bash permission → `/feed` frame shows
   `questionPendingAt` + `/output` shows `source:"hook"` **before** the
   Notification raise (~6s earlier than the old path).
2. Answer the dialog locally → the pending marker clears on the next frame
   (connection-drop path).
3. With `feed.allowPermissionAnswers: true`, POST
   `{"mode":"decision","decision":"allow","ifAskedAt":…}` → dialog dismisses
   in the pane, command runs, marker clears, pane ring drops.
4. AskUserQuestion in a bypass-permissions automation pane → hook-sourced
   choice body with digits/otherKey; answer via `mode:"keys"` as today.
5. Quit fly with a dialog pending → the hook process exits promptly (R9),
   the dialog still answerable in the orphaned pane's claude.

## Residual / deferred

- **Raising attention at PermissionRequest time** (a ~6s-earlier ring, and a
  ring for AskUserQuestion which today fires no Notification at all on some
  paths) — deliberately out of scope (KTD6); would touch suppression policy.
- **`updatedInput` / `addPermissionRuleOnAllow` / `updatedPermissions`** on
  remote allow — v1 answers are plain allow/deny.
- **Headless monitors** (stage 3 of the assessment) and a **`/focus` route**
  (stage 4) — separate plans.
- **`PermissionRequest` reliability instrumentation** (#58909-class misses):
  the fallback chain remains in place precisely because hook fire is not
  contractual; no alerting added.
