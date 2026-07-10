<!--
Plan: feed-question-screen-fallback
Status: implemented (see README.md index)
IDs (KTD/R/U) are scoped to THIS plan (repo convention: per-plan numbering).
-->

# feat: Screen-derived fallback for pending questions (`feed-question-screen-fallback`)

## Summary

Claude Code v2.1.206 stopped flushing a pending interaction's `tool_use` to the
transcript at ask time — the assistant entry (with the `AskUserQuestion` /
permission `tool_use`) is written only when the turn *resolves*. The
feed-pending-question surface rested on that flush (its KTD2 backward walk),
so on 2.1.206 `questionPendingAt` stays null and `/agents/{key}/output` serves
no `question` while a dialog sits open. Verified live 2026-07-10: a pending
AskUserQuestion sat 10+ minutes with **no assistant entry at all** in its
transcript; an exhaustive disk sweep found the question body nowhere on disk.
While pending, the body exists in exactly two places: Claude Code process
memory, and the PTY byte stream fly itself owns.

This plan adds a **screen-derived fallback**, layered strictly *behind* the
transcript scan: when the transcript abstains but the pane is corroborated as
waiting on an interaction (attention raise + Claude's live sessions file), the
question is synthesized from the pane's rendered terminal output — a bounded
raw-byte tail ring per pane, parsed on demand through a minimal VT
interpreter into a grid, then pattern-matched against Claude Code's picker
shape with KTD2-style abstain-on-surprise. Even when body parsing abstains,
the pending *signal* still surfaces (two-tier degrade).

## Problem Frame

- The transcript scan (`session/transcript.rs::pending_interaction_from_str`)
  is correct but now **blind at ask time** on 2.1.206. Its recorded contract
  ("the ask's `tool_use` flushes at ask time", verified 2026-07-06 on 2.1.200)
  no longer holds. This is arguably an upstream regression (transcript-based
  integrations silently broke between 2.1.200 and 2.1.206); the transcript
  scan must stay primary so a future Claude Code that resumes flushing at ask
  time restores full fidelity with zero changes here.
- What v2.1.206 still provides at ask time (all verified live 2026-07-10):
  1. The **Notification hook** (`fly notify --claude`): fires at ask time with
     `session_id`/`cwd`/`notification_type` — attention still raises. (For an
     AskUserQuestion the live sessions file labels the wait
     `"permission prompt"`, so the hook's `notification_type` may map to
     `Reason::Permission` even for a choice picker — the fallback must not
     trust the reason to discriminate kind.)
  2. **`~/.claude/sessions/<pid>.json`** — live per-session state:
     `{sessionId, cwd, status: "waiting", waitingFor, statusUpdatedAt, ...}`.
     A poll-able pending/cleared signal joinable to the resume store via
     `sessionId`. No question body. Caveat verified live: an entry can say
     `waiting` while its pane is long gone from fly's roster (the process
     outlives the pane's publication), so it is **corroboration only**, never
     an existence authority.
  3. The pane's **terminal grid**: the dialog as rendered text (numbered
     options, `❯` cursor, footer hint) — in the PTY byte stream fly's own read
     thread already handles.

## Requirements

- **R1 — transcript primacy.** When the transcript scan yields a pending
  question, behavior is byte-identical to today. The fallback engages only
  when the transcript abstains.
- **R2 — two-tier degrade.** The pending SIGNAL (frame `questionPendingAt`)
  surfaces whenever the pane is corroborated waiting, independent of whether
  the body parse succeeds. Body extraction gates only the `question` object,
  never the pending flag.
- **R3 — abstain-on-surprise body parse.** The screen parser targets Claude
  Code's picker shape. Anything unexpected — wrapped/scrolled-off options,
  multiple numbered blocks, non-1-based or non-contiguous numbering, zero or
  multiple `❯` cursors, unrecognized VT sequences that could corrupt the grid
  — yields "pending, body unavailable" (tier 1), never a guessed body.
- **R4 — digit fidelity.** Options are exposed with the numbers **as
  rendered** (no renumbering, no gap-closing). A screen-derived `answerable`
  question's option `key`s are by construction the digits keys-mode will
  deliver.
- **R5 — timestamp discipline + provenance.** A screen-derived question's
  `askedAt` comes from the ask-time raise stamp (backend wall clock at the
  Notification raise; fallback: the sessions file's `statusUpdatedAt`), never
  from any transcript value. The question carries its provenance
  (`source: "screen"` on the wire; transcript-derived bodies are unmarked).
  The answer path's `ifAskedAt` guard compares against the *currently served*
  question's `askedAt`, so a late transcript flush — which replaces the
  screen-derived question with a transcript-derived one under a different
  stamp — makes an in-flight screen-stamped answer 409, never mis-deliver.
- **R6 — existing gates keep holding.** `feed.allowPermissionAnswers` (with a
  widened trigger, KTD6), the per-leaf answered latch, the mandatory
  `ifAskedAt` for keys mode, and the R8/KTD7 `clean` pipeline
  (sanitize → scrub → truncate) apply to every screen-derived string.
- **R7 — bounded, always-on capture; zero cost while idle.** The per-pane
  output tail ring is a fixed-size memcpy tee on the read thread (no lock
  shared with the byte path's consumers beyond its own), and the VT parse runs
  only while a fallback is actually needed, cached against the ring's write
  sequence (a static dialog re-serves the cached parse).
- **R8 — honest wire shape.** No body → no `question` key (existing R5
  posture); a body-less pending is representable purely by the frame marker.
  Consumers render "question pending · answer at the terminal" from the
  marker alone.

## Key Technical Decisions

- **KTD1 — grid source: pane-owned raw tail ring + on-demand scoped VT parse.**
  Three candidates were weighed:
  - *(a) Headless VT emulator per pane, fed from spawn.* Rejected: VT state is
    cumulative, so the emulator must consume every byte of every agent pane
    forever — a per-byte tax on the hot read path and a persistent per-pane
    memory cost — to answer a question that arises rarely. It is also the
    largest new dependency surface.
  - *(b) Snapshot the frontend's xterm.js grid on a raise, push over IPC.*
    Rejected: it makes the webview a data source for a backend HTTP endpoint.
    The feed must serve while the window is hidden/throttled (WebKit throttles
    background webviews — already bitten once), during webview hangs, and the
    push would race the dialog's redraw. Backend surfaces stay backend-fed.
  - *(c — chosen) Bounded raw-byte ring + tail parse on demand.* The read
    thread tees each chunk into a fixed 64 KiB ring (the same pattern as the
    existing work-stretch recorder: a small side-effect after the bytes are
    already out). When a fallback is needed, the ring is linearized and played
    through a minimal VT interpreter into a fresh grid. Starting from a blank
    grid mid-stream is sound **for this use** because Claude Code's Ink UI
    repaints the whole dialog component per frame (cursor-up + erase-line +
    rewrite): the last full repaint is in the tail, and the parser matches
    *content patterns*, not absolute screen coordinates. Anything that would
    have needed earlier state trips the interpreter's surprise flag → abstain
    (R3). Cost while idle: one memcpy per read chunk.
- **KTD2 — `vte` for tokenization, our own ~minimal grid for semantics.**
  Hand-rolling a VT tokenizer (UTF-8 + CSI/OSC/DCS framing) is the error-prone
  part; `vte` (alacritty's parser, tiny, no runtime deps) does exactly that.
  The `Perform` impl maintains a line grid with only the semantics Ink-style
  redraws use (print with wrap, CR/LF/BS, CUU/CUD/CUF/CUB/CUP/CHA/VPA, EL/ED,
  SGR + mode sets ignored); any *layout-mutating* sequence outside that set
  (scroll regions, IL/DL/ICH/DCH, alternate screen, …) sets a `surprised`
  taint that forces abstention. The grid is row-capped (keep the last N rows)
  so a garbage flood can't balloon memory.
- **KTD3 — the picker matcher is shape-strict.** One numbered block of
  consecutive lines `[❯] <n>. <label>`, digits exactly `1..=N` in order,
  exactly one `❯`, block at/near the grid tail, a question line above it.
  Indented non-numbered lines inside the block fold into the preceding
  option's description. The permission dialog (also a numbered picker in
  current Claude Code) is classified by its canonical shape (leading
  "Do you want …" line / yes-no option texts). Any deviation → tier 1. The
  matcher is pinned by **real captured renders** (fixtures at two widths) plus
  adversarial fixtures that must abstain.
- **KTD4 — corroboration chain for the fallback trigger.** The fallback
  engages only when ALL hold: (1) transcript scan abstained; (2) the leaf is
  on the published roster (existing 404 authority); (3) its live attention
  reason is `question` or `permission`; (4) Claude's sessions file has a
  `status: "waiting"` entry for the leaf's resume-store `sessionId`. (3)
  bounds staleness by fly's own state (typing clears it); (4) bounds it by
  Claude's (answering flips it to `busy`/`idle`). The sessions file alone is
  never sufficient (verified live: it can say `waiting` for a pane fly no
  longer serves).
- **KTD5 — ask-time stamp registry.** The dispatch already sees the
  Notification raise; a new leaf-keyed `PendingSignals` registry stamps
  wall-clock ms on every recordable `Question`/`Permission` raise. That stamp
  is the screen-derived `askedAt` and the frame's tier-1 `questionPendingAt`.
  If no raise stamp exists (e.g. a hook variant that doesn't fire), the
  sessions file's `statusUpdatedAt` is the fallback stamp — stable while
  waiting (verified live). Stamps are overwritten by the next raise, never
  explicitly cleared: exposure is gated by KTD4's live corroboration, so a
  stale stamp is inert. A re-notify for the same dialog may move the stamp;
  the consequence is a 409 on an in-flight answer (safe direction).
- **KTD6 — answer-path posture for screen-derived questions.** A
  screen-derived question is `answerable` only in the one v1 shape (single
  single-select block, no parse drops, fully confident). The permission
  opt-in (`feed.allowPermissionAnswers`) is required for a guarded answer
  when EITHER the question's classified kind is `permission` OR the pane's
  live reason is `permission` — belt and braces, because 2.1.206's
  notification types blur ask-vs-permission and a misclassified permission
  dialog answered without opt-in would be remote tool approval. (This can
  gate a genuine ask behind the opt-in when Claude labels it a permission
  prompt; that is the accepted conservative direction.)
- **KTD7 — one fallback composition point.** `ReplyResolver` stays
  transcript-pure. A new `FallbackResolver` wraps it behind the same `IoFn`
  seam: transcript io first, fallback synthesis second, one deterministic
  result per call. The screen parse is cached keyed by `(leaf, ring seq)`;
  the frame emit and `/output` and the answer guard all read the same
  composed resolution, so the guard can never compare against a different
  source's stamp than the one served (R5).

## High-Level Technical Design

```
read thread (pty/pane.rs)
  ├─ sink(bytes)                       (unchanged)
  ├─ activity.record(n)                (unchanged)
  └─ tail_ring.write(bytes)  ──────────────┐  64 KiB, seq counter
                                           │
Notification raise (lib.rs dispatch)       │
  └─ PendingSignals.stamp(leaf, now_ms)    │  ask-time wall clock
                                           │
IoFn seam (lib.rs) → FallbackResolver.resolve_io(leaf)
  1. ReplyResolver.resolve_io(leaf)        │  transcript primary (R1)
  2. question None?                        │
     roster reason ∈ {question,permission}?│  (server passes reason in)
     sessions file waiting for sessionId?  │  session/livestate.rs
  3. yes → screen_fn(leaf) ────────────────┘
     parse ring tail (feed/screen.rs, vte grid, KTD2/KTD3)
       ├─ confident picker  → QuestionBody{source:"screen", askedAt=stamp}
       └─ surprise/abstain  → pending_at only (tier 1)
  4. emit_frame: questionPendingAt = question.askedAt ∥ pending_at
     /output: question object only when body exists (R8)
     input guard: same composed resolution → askedAt identity (R5)
```

## Implementation Units

### U1. Per-pane output tail ring + dims (`pty/pane.rs`, `pty/mod.rs`)
A fixed-capacity byte ring (`TAIL_RING_CAP = 64 KiB`) in `PaneShared` behind
its own `Mutex`, written by the read thread after the sink send; a monotonic
`seq` (total bytes written) identifies content versions. `PaneShared` also
tracks the last-known `(rows, cols)` (set at spawn, updated by `resize`).
Accessors: `Pane::screen_tail() -> ScreenTail { bytes, seq, rows, cols }` and
`PtyManager::screen_tail_by_leaf(&str)`. Tests: wrap-around linearization,
seq monotonicity, dims update on resize.

### U2. Minimal VT grid + picker parser (`feed/screen.rs`, new dep `vte`)
Pure module, fixture-driven. `render_tail(bytes, cols) -> Grid` (KTD2
semantics + `surprised` taint, row cap `MAX_GRID_ROWS = 200` keeping the
tail). `parse_interaction(&Grid) -> Option<ScreenInteraction>` (KTD3):
`ScreenInteraction { kind: Choice|Permission, question, header, options:
Vec<(rendered_digit, label, description)>, cursor_at }` — digits as rendered
(R4). Abstains on: taint, no block, >1 block, digits ≠ 1..=N, ≠1 cursor,
no question line, any suspicion of truncation (block starts at grid top —
options may be scrolled off). Fixtures: real captured AskUserQuestion +
permission renders at 80 and 120 cols, plus adversarial must-abstain cases.

### U3. Ask-time stamp registry (`feed/pending.rs`)
`PendingSignals`: `Mutex<HashMap<leafKey, u64>>`; `stamp(leaf, now)`,
`get(leaf)`. Managed unconditionally in `lib.rs`; the dispatch stamps on
every recordable `Question`/`Permission` raise (after the capture-only and
automation-suppression early returns), resolving `PaneId → leaf_key` the
same way resume capture does. No explicit clear (KTD5).

### U4. Live sessions-file reader (`session/livestate.rs`)
`waiting_state(sessions_root, session_id) -> Option<WaitingState { waiting:
bool, status_updated_at_ms: u64 }>`: scan `~/.claude/sessions/*.json`
(root injected for tests), parse defensively (serde, unknown fields
ignored), match by `sessionId`. `None` on unreadable dir / no match — the
caller treats that as "not corroborated". fly reads only; never writes under
`~/.claude`.

### U5. Fallback composition (`feed/fallback.rs`)
`FallbackResolver` wrapping `ReplyResolver` (KTD7) with injected seams:
`screen_fn`, `signals: Arc<PendingSignals>`, `sessions_root`, plus the
resume path (to join leaf → sessionId). Output widens `ResolvedIo` with
`pending_fallback_at: Option<u64>` (tier 1). Applies the `clean` pipeline +
count ceilings to every screen string via the existing `question_body`-style
shaping; `QuestionBody.source = Some("screen")`; `askedAt` per KTD5. Parse
cache keyed `(leaf, seq)`. Unit tests: transcript wins; fallback only under
full KTD4 corroboration; body-abstain still yields `pending_fallback_at`;
stamp preference (raise stamp over `statusUpdatedAt`); determinism.

### U6. Wire + server surfaces (`feed/wire.rs`, `feed/server.rs`, `src/lib/feed.ts`)
`QuestionBody.source: Option<String>` (`skip_serializing_if` none — old
bodies byte-identical). `emit_frame` stamps `questionPendingAt =
gated_question(..).askedAt ∥ (pending_fallback_at when reason ∈ {question,
permission})`. `gated_question` unchanged for transcript bodies; a
screen-derived body is exposed under the same rule keyed on its classified
kind. Answer path: the KTD6 widened opt-in trigger (kind == permission OR
live reason == permission, for screen-derived bodies). `feed.ts` mirrors
`source`. Tests: gating matrix, opt-in widening, 409 on stamp change
(transcript flush replacing a screen body).

### U7. Wiring (`lib.rs`)
Manage `PendingSignals`; stamp in dispatch (U3); construct
`FallbackResolver` in the feed block with `screen_fn` from `PtyManager`,
and point `IoFn` at it. No change when the feed is disabled.

### U8. Fixtures + live verification
Capture real renders by driving `claude` in a scratch PTY (AskUserQuestion
via prompt; a Bash permission dialog in default mode) at 80 and 120 cols;
commit raw captures under `src-tauri/tests/fixtures/screen/`. Live pass
against the running app: pending ask → marker + body on `/output`;
keys answer lands; permission dialog → opt-in gate; transcript-flush
takeover → 409 for the stale stamp.

### U9. Docs + upstream note
Update `CLAUDE.md` (feed section), `docs/plans/README.md`, and the
now-false contract comment at `session/transcript.rs` (KTD2 note: record the
2.1.206 regression and that the screen fallback exists). Draft an upstream
report (transcript ask-time flush regression) in the plan's Open Questions.

## Open Questions

- Does 2.1.206 fire a Notification hook for AskUserQuestion (not just
  permissions)? The sessions file's `waitingFor: "permission prompt"` for a
  live ask suggests yes-with-permission-type; only a live run confirms which
  `Reason` fly records. The design tolerates both (KTD5 stamp fallback,
  KTD6 belt-and-braces gating), but the live pass should pin it.
- Upstream: file the regression (transcript `tool_use` no longer flushed at
  ask time; `Stop`-adjacent flush only) — the transcript scan self-heals if
  fixed.

## Scope Boundaries

- No frontend UI for the fallback (the feed consumer is external); only the
  `feed.ts` type mirror.
- No OSC/Bel attention tiers, no non-Claude TUIs: the parser targets Claude
  Code's picker only and abstains elsewhere.
- The ring is not scrollback and is never persisted.
- Free-text "Other" flows stay hindsight-only: keys-mode answers digits.

## Risks

- **Claude Code TUI redesign** silently degrades body fidelity → tier 1
  (pending-only), by construction. Fixtures make the supported shape explicit.
- **Misclassification risk** (ask vs permission) is bounded by KTD6's widened
  opt-in; the failure direction is "asks need the opt-in", never "permissions
  bypass it".
- **Ring taint from concurrent output** (spinner frames after the dialog):
  Ink repaints whole frames, so the last repaint wins in the grid; if a
  partial trailing frame confuses the matcher, it abstains (R3).

## Acceptance Examples

1. Pending ask on 2.1.206, transcript empty: frame shows
   `questionPendingAt = <raise stamp>`; `/output` serves
   `question { source: "screen", askedAt = <raise stamp>, options[].key
   as rendered }`; a keys `"2"` with `ifAskedAt = <raise stamp>` delivers.
2. Same, but the screen holds a wrapped 14-option monster the parser can't
   read: frame still shows `questionPendingAt`; `/output` has no `question`.
3. Permission dialog, `allowPermissionAnswers = false`: body (if parsed)
   serves read-only; a guarded answer → 403 discriminator.
4. A future Claude Code flushes at ask time again: transcript scan wins,
   `source` disappears, screen path never engages (R1).
