---
title: "feat: Feed agent reply read + input write endpoints"
type: feat
date: 2026-07-05
status: built
depth: compact
origin: external consumer request (as-built record)
---

# feat: Feed agent reply read + input write endpoints (`feed-agent-reply-io`)

## Summary

Three additions to the existing local feed HTTP server
(`2026-07-04-001-feat-agent-state-local-feed`), requested by its consumer (a
3D portfolio) so each agent tile can show the agent's **latest reply**
and **send a prompt/answer back**:

1. `GET /agents/{key}/output` — the agent's latest textual reply
   (`{"text", "repliedAt"}`; `{"text": ""}` when it has never replied).
2. `POST /agents/{key}/input` — submit `{"text"}` to the agent, equivalent to
   typing it into the pane and pressing Enter (`200 {"ok": true}`).
3. `lastReplyAt` (epoch ms | null) on every agent object in `/feed` frames.

`{key}` is the roster's `leafKey`, verbatim. Same server, same loopback bind,
same bearer token as `/feed` — no new listener. Status contract the consumer
maps: 2xx ok · 401/403 auth · 404/501 not-available/unknown; plus 400 for a
malformed input body and 500 for a failed PTY write.

## Requirements

- **R1**: Both endpoints authenticate with the exact `/feed` bearer check
  (constant-time, silent bare-status rejection).
- **R2**: An unknown/gone `{key}` is 404; a known agent with no reply yet is
  `200 {"text": ""}` — the consumer renders "no reply" and offers to send one.
- **R3**: `lastReplyAt` **equals** `/output`'s `repliedAt` for the same reply —
  both must come from one resolver, or the consumer's unread-dot arming
  (newer `lastReplyAt` ⇒ unread; cleared by reading the reply) breaks.
- **R4**: Input delivery behaves exactly like local typing: bracketed-paste
  wrapped, control-stripped, `\r`-submitted, and it clears the pane's
  attention the way `pty_write` does.
- **R5**: The write route must not widen the boundary beyond "send text":
  no raw control bytes, no paste-marker forgery, capped body (64 KiB).

## Key Technical Decisions

- **KTD1 — reuse the feed server.** New routes on the existing
  `tiny_http` accept loop; auth now precedes routing on everything but
  `/healthz`, so an unauthenticated caller cannot map the surface.
- **KTD2 — the published roster is the key authority.** `{key}` is known iff
  it is in the currently-pushed `FeedState` roster (`agent_exists`). Only
  agent panes are ever published, so a bare shell is never remotely
  addressable; a key the consumer got from `/feed` resolves symmetrically.
- **KTD3 — one deliberate mutation route.** `/input` is the feed's first
  write. The paste is built by `feed::io::paste_payload` — the exact
  `lib/handoff.ts::injectionPayload` normalization (CR/CRLF→LF, all other
  control chars stripped, ESC included) — and the submit `\r` is a **separate
  PTY write ~150ms later**: Claude Code's composer treats bytes in the same
  chunk as the paste as pasted content, so a same-chunk `\r` becomes a
  composer newline, never a submit (found live against 2.1.201). A token
  holder can say things *to* an agent; it cannot inject terminal control
  sequences, resize/kill panes, or touch non-agent panes.
- **KTD4 — replies resolve backend-side at emit time, with a settle bump.**
  `lastReplyAt` is stamped on each frame in `emit_frame` (the pushed roster
  never carries it). Claude Code flushes the final assistant turn ~100ms
  *after* the Stop hook (see `stop-hook-precedes-transcript-flush`), so the
  frame emitted on the immediate status change can read the previous reply;
  `lib.rs`'s hook dispatch schedules one delayed (2s) `FeedState::bump` per
  Stop so connected consumers get a corrected frame after the flush.
- **KTD5 — resolve via the resume store, cached by transcript identity.**
  leaf → `ResumeRecord{session_id, session_cwd}` → transcript path → last
  assistant turn (`transcript::last_assistant_reply`, text + that turn's
  timestamp). The resume store is durable, pane-precise (Hook/Pick-ranked),
  and its ids are validated at write time so the joined path stays under the
  projects root. A `(path, mtime, len)` cache keeps multi-MB transcripts from
  re-parsing on every frame × consumer.

## Implementation Units

- **U1** — `feed/wire.rs`: `AgentEntry.last_reply_at` (`#[serde(default)]`,
  null = never replied) + `AgentOutputBody {text, repliedAt?}`; mirrored in
  `src/lib/feed.ts` (frontend always pushes null).
- **U2** — `session/transcript.rs`: `LastReply {text, replied_at_ms}` +
  `last_assistant_reply` — the existing last-assistant scan now pairs the
  text with its own turn's timestamp; `last_assistant_text` delegates.
- **U3** — `feed/io.rs`: `ReplyResolver` (KTD5), the one source behind both
  reply surfaces (R3). Reply text is `sanitize_multiline`d (R16 posture);
  a reply that sanitizes to blank counts as absent.
- **U4** — `feed/server.rs`: `ReplyFn`/`InputFn` seams, auth-first routing,
  the two `/agents/{key}/…` routes and their status mapping, frame
  enrichment in `emit_frame`.
- **U5** — `feed/io.rs::{paste_payload, SUBMIT, SUBMIT_DELAY}` +
  `pty::PtyManager::pane_by_leaf` (live-gated reverse lookup, newest id wins)
  + the `lib.rs` input closure (paste write → delayed Enter write → the
  `pty_write` attention clear; the delay rides the HTTP connection thread).
- **U6** — `lib.rs`: post-Stop delayed feed bump (KTD4).

## Scope Boundaries

Not here: multi-turn reply history (only the latest turn), reply streaming,
per-consumer read cursors (the consumer keeps its own unread state), any
route that manages panes/automations, non-loopback binds, CORS (the consumer
is not a browser-origin caller today).
