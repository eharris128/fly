# fix-feed-question-detection-gaps — root cause & fix (2026-07-11)

Reference note for the fix on branch `fix/feed-question-detection-gaps`. Not a
plan: a bug post-mortem. The change is cross-referenced from code comments as
**fix-feed-question-detection-gaps** and layers on
`2026-07-10-001-feat-feed-question-screen-fallback-plan.md` (KTD4/KTD5/R2/R5).

## Symptom

An agent pane (leaf-18, "p2") sat blocked on a *re-asked* AskUserQuestion
picker — "What is your name? (Alex / Alex Morgan / Something else)" below a
"User declined to answer questions" line — while the feed reported
`status:"idle", needsAttention:false, questionPendingAt:null` and
`GET /agents/{key}/output` served `{"text":""}`. Every downstream consumer
missed the blocked agent.

## Root cause — three stacked detection failures

1. **Transcript blind (expected, known).** Claude Code ≥ 2.1.206 flushes an
   ask's `tool_use` only at turn resolution. Primary scan abstains — this is
   exactly the gap the screen fallback exists for.

2. **The fallback's reason gate conflated attention with blockage.** The old
   gate required live roster reason ∈ {question, permission}. But:
   - AskUserQuestion **fires no hook at all** (pinned in `lib.rs` dispatch), so
     an ask never raises attention by itself; and
   - a raise on a *visible* pane is instantly **Acknowledged**
     (`state/attention.rs` — you're looking at it), and `home.ts` pushes
     `reason` only while `att === "raised"`.
   So a re-asked or merely-glanced-at picker has `reason: null` forever, and
   the fallback never engaged. This gap exists for *every* pane, not just the
   exotic one below — a transcript-derived choice question stays exposed
   regardless of reason (`gated_question`), so the fallback was strictly
   out of contract.

3. **Child-session claude: no livestate, no transcript, ever.** The dev-flavor
   fly (`pnpm flavor:dev`) was launched from inside a Claude session, so its
   env carried `CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`,
   `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_ENTRYPOINT`. Every pane inherited
   them, and every `claude` in those panes ran as a **child session**.
   Verified live (2.1.207, PTY probe): a child-session claude writes **no
   `~/.claude/sessions/<pid>.json`** and **flushes no transcript at any
   point** (not at ask, not at turn end, not at answer). That silently blinds
   not just this fallback's corroborator but resume attribution, automation
   output capture, and handoff qualification for dev-flavor panes.

Also verified by probe: declining a picker (Esc) collapses it to a digit-free
summary line ("User declined to answer questions · What is your name? (…)"),
so a re-asked picker is the *only* numbered block on the grid — the strict
`screen.rs` parser matches it (fixture:
`tests/fixtures/screen/ask-declined-reask-80.raw`).

## Fix

- **`feed/fallback.rs`** — gate rebuilt around the sessions file, three-valued:
  `waiting` → engage (tier 1 + tier 2), **reason no longer required** and not
  status-gated (the picker's own draw keeps the pane `working` for the 75 s
  activity gap; detection must not wait it out); explicitly not-waiting →
  abstain (Claude's live word wins); **no entry** → the strict screen parse is
  the sole authority: engages only for a non-`working` pane (a raised
  question/permission reason accelerates past that), and exposes *nothing* —
  no bare tier-1 stamp — without a fully parsed body.
- **askedAt discipline** — a raise stamp counts only when it postdates the
  corroborator's own stamp (stamps are never cleared, so an old dialog's stamp
  must not leak onto a new one); fallbacks: `statusUpdatedAt` (waiting leg) or
  the tail ring's new `last_write_at_ms` (no-entry leg — a parked dialog
  produces no output, so the last write is the draw).
- **`pty/pane.rs`** — `TailRing`/`ScreenTail` carry `last_write_at_ms`
  (time-injected); pane spawn **strips the Claude session-identity env
  markers** so a claude in a fly pane is always a top-level session.
- **Seam** — `IoFn` carries `(leaf, reason, status)`; `FeedState::agent_gate`
  replaces `agent_reason`, returning reason + status from one roster snapshot.

`needsAttention`/`status` semantics are unchanged on purpose: they reflect
attention (acknowledged = seen), while `questionPendingAt` + the `/output`
question body now reflect blockage, matching the transcript-derived contract.

## Known limitation (accepted)

The picker's 4th option — the appended **"Type something."** free-text user
input row — does not work in this scenario (verified by the maintainer post-fix).
Accepted, not blocking: the marker, question body, and the ordinary option
digits are the fixed contract; the free-text row's remote path
(feed-other-answer `mode:"other"`) stays guarded exactly as before — a
consumer that needs it should fall back to answering with a listed option.
