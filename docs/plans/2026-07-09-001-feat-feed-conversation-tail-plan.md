# feat: conversation tail on the agent output endpoint (`turns`)

**Status:** shipped
**Origin:** consumer request from the garden (the walking-sim cockpit reading
fly's feed) — its per-agent reader is growing from "latest reply only" into a
short speaker-labeled conversation tail. Contract growth on an existing
endpoint: no new endpoint, no transport or auth change, no new retention
(Claude Code's transcript `.jsonl`, which the reply resolver already reads and
caches, IS the history store).

## Contract

`GET /agents/{key}/output` gains one optional field:

```
turns?: [ { role: "user" | "agent", at: <epoch ms>, text: string }, ... ]
```

## Requirements

- **R1** — `turns` rides `GET /agents/{key}/output` only; every other byte of
  the response (and every other surface: `/feed` frames, input route) stays
  identical to before.
- **R2** — `role` is exactly `"user"` (a prompt delivered TO the agent, from
  any source) or `"agent"` (a reply FROM it); `at` is epoch ms (`repliedAt`'s
  convention) and mandatory per turn — a stampless turn is dropped, never
  served unstamped.
- **R3** — turns are ordered oldest → newest and the array ends with the
  current reply: the final turn's `at` equals `repliedAt` (the consumer's
  correlation contract).
- **R4** — pinned serving ceilings (`feed/io.rs`): depth ≤ **12 served turns**
  (`MAX_TURNS`), per-turn text ≤ **2048 chars** + ellipsis (`TURN_CAP`; ≤
  4·2048 bytes of UTF-8 — the same order as the 8 KiB
  `automations::model::OUTPUT_TAIL_CAP_BYTES` reference). Truncate-and-serve,
  no wire marker — the question-string contract.
- **R5** — degradation is omission: no stamped reply, no servable history, or
  everything dropped ⇒ the `turns` key is **absent**, never an empty array or
  null.
- **R6** — a *user* turn is any text-bearing prompt regardless of source
  (terminal, feed `POST /input`, anywhere). Tool chatter is excluded
  structurally: `tool_result`-only user entries (tool returns AND remote
  `mode:"keys"` digit answers) carry no text; a permission approval writes no
  user entry at all. Sidechain, `isMeta`, and `isCompactSummary` user entries
  are excluded. An *agent* turn is one working stretch's **final** text entry:
  a run of assistant text entries uninterrupted by a user prompt collapses to
  its last (KTD1) — the mid-run narration between tool calls is intermediate
  output, not conversation. (Caught by live verification on a real
  transcript: without the collapse, narration filled all 12 slots and crowded
  out every prompt.)
- **R7** — every served turn text passes the full `clean` pipeline (sanitize →
  secret-scrub → truncate) — deliberately **stricter** than the sibling
  `text` field's legacy no-scrub posture, since turns are newly exposed
  strings (feed-pending-question R8/KTD7 posture).

## Key technical decisions

- **KTD1 — reply-predicate parity.** The tail scan's per-entry agent
  predicate is *exactly* `last_assistant_reply_from_str`'s (any
  `type == "assistant"` entry yielding `assistant_text_blocks`, no
  sidechain/meta filtering), and the R6 run-collapse keeps each run's
  **newest** text entry — so the scan's newest agent turn IS the entry
  `text`/`repliedAt` are served from and R3 holds by construction, not by
  comparison. A defensive stamp check in `shape_turns` abstains (serves
  nothing) if they ever diverge.
- **KTD2 — serve only alongside a stamped reply.** Prompts newer than the
  last reply (the agent is still working on them) are cut; they surface once
  the next reply closes them out. No reply, or a stampless one ⇒ omit. This
  keeps the "array ends with the reply" invariant total.
- **KTD3 — bounded raw window.** The transcript scan retains only the
  trailing `RAW_TURN_BUFFER = 24` raw turns (2× the serving depth, statically
  asserted in `feed/io.rs`), so a multi-MB transcript never materializes as
  an unbounded turn list; the resolver's existing `(path, mtime, len)` cache
  (feed-agent-reply-io KTD5) makes re-reads free when unchanged.

## Units

- **U1** — `session/transcript.rs`: `RawTurn`/`TurnRole`,
  `conversation_turns_from_str` (bounded, oldest → newest), `user_text_blocks`;
  `TranscriptIo` gains `turns` (same single read).
- **U2** — `feed/io.rs`: `MAX_TURNS`/`TURN_CAP`, `shape_turns` (cut at the
  reply → newest-12 window → clean each), `ResolvedIo.turns` (cached with the
  reply/question, one source for every surface).
- **U3** — `feed/wire.rs` `TurnEntry` + `AgentOutputBody.turns`
  (`skip_serializing_if = Vec::is_empty` ⇒ R5 omission); `feed/server.rs`
  rides it ungated (completed history, already scrubbed/capped);
  `src/lib/feed.ts` mirrors the shape.

## Consumer pinning (reported to the garden)

| Ceiling | Value |
|---|---|
| Serving depth | ≤ 12 turns |
| Per-turn text | ≤ 2048 chars (+ `…`), so ≤ 8195 bytes UTF-8 |
| `at` type | epoch milliseconds, unsigned integer (same as `repliedAt`) |

## Live caveats (inherited, not new)

- Turn timestamps come from the transcript entries' own ISO-8601 stamps
  through the overflow-guarded `iso8601_to_ms` (release builds wrap silently
  on overflow — the year bound is load-bearing).
- The reply flushes ~100ms after the Stop hook (see the
  stop-hook-precedes-transcript-flush memory); the tail lags the same way the
  reply itself does — the settle bump already re-emits.
