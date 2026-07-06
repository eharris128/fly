# Residual review findings — feat/feed-pending-question

Accepted-and-deferred findings from the multi-agent code review of the
`feed-pending-question` branch. Recorded per the ce-work Residual Work Gate
(user decision: *accept & record*). None is a correctness or security defect;
all applied review fixes landed in commit `01e065e`
(`fix(feed): apply code-review findings …`). This repo has no remote, so these
residuals live here rather than in a PR description.

**Review run:** `/tmp/compound-engineering/ce-code-review/20260706-124600-abfc5a55/`
(base `28d3836`). 10 structured reviewers (correctness, security, adversarial,
api-contract, reliability, testing, maintainability, project-standards) + 2 CE
agents (agent-native, learnings). Verdict: **Ready with fixes** — the applied
fixes cleared every correctness/security finding; the items below are the
deliberately-deferred remainder.

## Deferred P2 findings (surfaced to the user, accepted)

### R-1 — `resolve_io` holds the cache mutex across the full transcript parse
- **Where:** `src-tauri/src/feed/io.rs::ReplyResolver::resolve_io`
- **Reviewer:** reliability (P2, anchor 75); echoed by correctness/adversarial
  as a residual risk.
- **What:** On a cache miss the one global cache mutex is held across the whole
  `transcript_io` parse (reply scan **plus** the new pending backward-walk) over
  a file the code's own doc says can be multi-MB. Concurrent SSE consumers and
  `/output` reads serialize behind that parse.
- **Why deferred:** KTD5 deliberately parses under the lock, and real contention
  is low for a single-consumer desktop feed (the `game` portfolio + a handful of
  agents). The fix (double-checked locking: check under lock → drop → parse →
  re-lock → insert) adds a possible redundant parse under race and changes a
  deliberate design decision. Revisit if a second live consumer or very large
  transcripts make the serialization observable.

### R-2 — `session/transcript.rs` file-size regression (1802 lines)
- **Where:** `src-tauri/src/session/transcript.rs`
- **Reviewer:** maintainability (P2).
- **What:** The pending-scan subsystem (`pending_interaction_from_str`, the
  `WalkItem` walk, `parse_questions`, and their tests) grew the file from ~1216
  to ~1802 lines; it is self-contained and extractable to its own module.
- **Why deferred:** The plan (U1) deliberately co-located the pending scan with
  its sibling transcript scanners (`scan_turns`, `last_assistant_reply`), which
  it shares helpers with (`iso8601_to_ms`, `assistant_text_blocks`). Extraction
  is a larger structural refactor better done as a deliberate split across the
  accumulating transcript scanners, not folded into this feature branch.

## Lower-severity residual risks (noted by reviewers, accepted as-is)

- **Latch / cache never evicted.** The per-leaf answered-latch `HashMap`
  (`feed/server.rs`) and the `ReplyResolver` cache grow one entry per distinct
  leaf key ever seen; no eviction on pane/roster removal. Bounded and small for
  a desktop session; no clean cross-struct hook to `FeedState::publish` today.
- **Notification settle-bump thread per hook.** `lib.rs` spawns one detached
  2s-sleeping thread per attention-raising Notification (not coalesced, not
  gated on `recordable`), mirroring the pre-existing post-Stop bump. Self-limiting
  (Claude Code blocks on the hook) and idempotent; a raise storm multiplies
  short-lived threads but has no correctness impact.
- **Scrub-before-sanitize reassembly still present in the automations capturer.**
  The reassembly leak fixed in `feed/io.rs::clean` (sanitize → scrub → truncate)
  also lurks pre-existing in the automations output capture path
  (`lib.rs` ~617–618, `automations`/`redact`). Out of this plan's scope; the
  automations store is `0600` and not externally reachable like the feed, but the
  same reorder would harden it. Follow-up.
- **Reply scan does not skip `isSidechain` entries** the way the pending scan
  does, so `/output` `text` beside a question can be a same-file subagent's text
  in sidechain-tailed transcripts (pre-existing reply behavior, now co-served on
  one `/output` response). U1's empirical check found no sidechain entries in
  current transcripts (subagents get separate files); revisit if that changes.
- **Choice questions are dormant on current Claude Code.** Upstream flushes an
  AskUserQuestion `tool_use` only *with* its result, so a pending choice is never
  seen on-disk today (documented abstain-degrade). When upstream flushes at ask
  time, `gated_question` does not corroborate a *choice* against a live reason —
  a remote-vs-local answer race would be guarded only by `askedAt` + the
  transcript-flush lag. Trust-rank gating of the read+answer surface (plan Open
  Question option b) is the durable close, deferred by the shipped option (a).
- **Cache key `(path, mtime, len)`** would serve stale content on an in-place
  same-length rewrite within mtime granularity — practically unreachable given
  append-only JSONL (inherited KTD5 assumption).
