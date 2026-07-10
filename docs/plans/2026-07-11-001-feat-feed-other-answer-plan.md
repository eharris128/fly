<!--
Plan: feed-other-answer
Status: implemented (see README.md index)
IDs (KTD/R/U) are scoped to THIS plan (repo convention: per-plan numbering).
-->

# feat: Remote free-text ("Other") answers to a pending picker (`feed-other-answer`)

## Summary

A feed consumer can select a pending AskUserQuestion's authored options
remotely (`mode:"keys"`), but it cannot answer with the picker's **free-text
row** — the "Type something." row Claude Code appends after the authored
options, which focuses an inline text input. Neither existing input mode can
do it safely:

- `mode:"submit"`'s payload **leads with an ESC byte** (the bracketed-paste
  start marker `ESC[200~`). At an unfocused picker that ESC reads as a bare
  Escape and **cancels the question**; the text then falls into the main
  composer and submits as an ordinary message ("User declined to answer
  questions"). Reproduced deterministically by the requesting consumer.
- `mode:"keys"` strips every control char (no Enter) and caps at 16 chars —
  it can select a digit, never type-and-submit a sentence.

This plan adds `mode:"other"`: the consumer posts
`{text, mode:"other", ifAskedAt}` and **fly owns the keystroke choreography**
— the free-text row's digit, the sanitized text, and a real Enter, delivered
as three separate, delay-spaced PTY chunks. No chunk ever contains an ESC
byte, so nothing can read as a cancel. The row's digit rides the wire as
`QuestionSpec.otherKey` so the consumer knows *whether* an Other answer is
possible without guessing keybindings (agent-native parity with `options[].key`).

## The picker contract (probed live, 2026-07-10, Claude Code 2.1.206)

Driven against a real AskUserQuestion picker (2 authored options) via a raw
PTY (`scratchpad/otherprobe/probe.py`; capture retained during review):

1. **The Other digit focuses the input directly** — typing `3` (= authored
   count + 1) switched the row to a focused inline input
   (`❯ Type something.` + `ctrl+g to edit in VS Code · Esc to cancel`
   footer). No Enter needed first.
2. **Raw text lands verbatim** when written as its own chunk after the digit;
   a `\r` written as a third chunk **submits it as the Other answer** (the
   tool result carried the typed sentence exactly).
3. **Raw text at an unfocused picker is ignored** — there is no type-ahead
   auto-focus — and a following Enter **selects the highlighted default**
   (the answer came back "Red"). A no-digit "type" mode is therefore unsafe.
4. **A digit coalesced with the text into one chunk is dropped** — the picker
   processed neither the selection nor the text, and the trailing Enter again
   selected the default. Chunk boundaries + gaps are load-bearing.
5. **Bracketed paste (`?2004h`) is enabled once at startup and reset only at
   exit** — it is composer-global, never picker-scoped, so there is no mode
   signal that could make a paste-based delivery safe; safety depends
   entirely on focus, which is unknowable from outside at write time.

(1)+(2) answer the consumer's open questions: the digit alone focuses the
input, and while paste works *once focused*, raw typing is strictly safer and
equivalent for a sanitized single line. (3)+(4) are why a consumer-driven
digit-then-text dance (two HTTP calls, or one coalesced write) can never be
reliable, and why fly must own the timing.

## Key Technical Decisions

- **KTD1 — fly owns a three-chunk, delay-spaced choreography.** Delivery is
  `digit` → sleep `SUBMIT_DELAY` → `text` → sleep `SUBMIT_DELAY` → `\r`, each
  as its own PTY write on the HTTP connection thread (same posture as
  submit's delayed Enter). The gaps defeat the live-probed coalescing drop;
  separate writes defeat the same-chunk swallow. No ESC anywhere: the text is
  raw (control-stripped), never paste-wrapped — the probe pinned that a paste
  is only safe when focus already holds, which cannot be verified remotely.
- **KTD2 — the Other digit is knowledge, not arithmetic at answer time.** It
  is resolved where the question body is built and carried on the wire as
  `otherKey`; the route refuses (409) when absent. For a **transcript**
  body it is *source option count + 1* — this rests on the picker's
  appended-row contract, verified on 2.1.206 and assumed for the 2.1.20x
  range fly targets (transcript bodies only exist at ask time ≤ 2.1.205,
  where the row's presence is unverified — see Residual risk). For a
  **screen** body the digit is read off the rendered "Type something." row
  itself — no assumption at all. Single-keystroke digits only (≤ 9).
- **KTD3 — reuse the guarded-answer machinery wholesale.** `mode:"other"` is
  a guarded delivery exactly like keys: mandatory `ifAskedAt`, the fresh
  reason re-read, the per-leaf answered latch (mode-agnostic: one delivery
  per `askedAt` across keys/other/guarded-submit), and the permission /
  screen-under-permission opt-in gate in the same pinned precedence slot.
  No new config.

## Requirements

- **R1** — `mode:"other"` without `ifAskedAt` is 400 (an unguarded free-text
  answer could type into whatever dialog happens to be up).
- **R2** — the wire's `QuestionSpec.otherKey` (camelCase, omitted when
  absent) is the one place a consumer learns the free-text row's digit; a
  transcript-derived answerable single-select carries source-count + 1.
  Old bodies without the key still deserialize (back-compat both ways).
- **R3** — a screen-derived choice body's `otherKey` is the rendered digit of
  exactly one row whose cleaned label is `"Type something."`; anything else
  (absent, duplicated, two-digit) leaves it unset — degrade to
  "not Other-answerable", never a guessed digit.
- **R4** — the route delivers an Other answer only against a choice-kind,
  `answerable` question with a known `otherKey`; every other shape is 409.
  Rationale: with no focused input the trailing Enter would select the
  highlighted default (probed) — refusing beats answering wrongly.
- **R5** — the text chunk is `other_payload`: newlines collapse to single
  spaces (the inline input is one line; a kept `\r` submits early, a dropped
  one joins words), then keys-grade char-level control stripping (no ESC, no
  smuggled submit). Blank-after-filter is 400, never an empty write.
- **R6** — sentence-scale cap `OTHER_MAX_CHARS` (512 chars): over-cap is
  rejected outright (400), never truncated into the pane (KEYS posture).
- **R7** — status precedence, latch semantics, and the permission opt-in are
  byte-identical to the existing contract; `submit` and `keys` behavior is
  untouched.

## Units

- **U1** — `feed/wire.rs`: `QuestionSpec.other_key` + round-trip/back-compat
  tests; mirrored in `src/lib/feed.ts`.
- **U2** — `feed/io.rs`: `OTHER_MAX_CHARS`, `other_payload`,
  `other_digit_after` + transcript-body `otherKey` stamping in
  `question_body`; tests (payload filtering, digit derivation, answerable /
  unanswerable stamping).
- **U3** — `feed/fallback.rs`: screen-body `otherKey` from the rendered row
  (`OTHER_ROW_LABEL`); tests against a render mirroring the real
  `ask-80.raw` extras shape.
- **U4** — `feed/server.rs`: `InputAction::Other {select, text}`, the
  `"other"` mode parse (400s) and the R4 guard-slot check (409);
  `tests/feed_server.rs` pins the full status contract (200/400/409/403,
  latch shared across modes, screen-under-permission gating, digit sourced
  from the question never the consumer).
- **U5** — `lib.rs` `input_fn`: the KTD1 three-chunk delivery; attention
  clears on delivery exactly like the other actions.

## Residual risk / open questions

- **≤ 2.1.205 transcript bodies.** Whether those pickers render the
  "Type something." row (and at count + 1) is unverified — no such binary is
  installable here. If a target version lacks it, a delivered Other answer
  degrades to the R4 failure the guard cannot see (Enter picks the default).
  If that matters, drop the transcript-side `otherKey` stamping (one line in
  `io.rs::question_body`) and Other answers remain available wherever the
  screen fallback corroborates the row — which on ≥ 2.1.206 is the only
  ask-time body source anyway.
- **Row label drift.** A future picker renaming "Type something." silently
  removes screen-side `otherKey` (safe direction). The transcript side would
  keep stamping count + 1; revisit `OTHER_ROW_LABEL` + `other_digit_after`
  together when bumping the pinned screen fixtures.
- **multiSelect / multi-question batches** stay out of scope (not
  `answerable`), same as keys.
