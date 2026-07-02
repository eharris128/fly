---
date: 2026-07-02
topic: session-handoff
---

# Session Handoff — Requirements

## Summary

Add a pane **handoff** action to fly: one leader chord splits a stale agent pane alongside and launches a fresh Claude Code instance pre-prompted to read the previous session's transcript and continue its outstanding work. A second chord does the same but leaves the prompt pre-typed and unsent, so extra direction can be appended before sending.

---

## Problem Frame

A Claude Code session runs for a while, the user walks away, and returns to a pane holding 200k+ tokens of mostly stale context. The remaining work rarely needs that history. Today the recovery ritual is manual: split the tab, launch a fresh `claude`, and type a handoff paragraph ("a previous instance mostly finished the work — take a peek and see what's next"). The typing is the friction — the ritual repeats on every return-to-desk, and even the command palette is more keystrokes than the moment deserves. Occasionally the user wants to add direction beyond "see what's next," but that is the exception.

---

## Key Decisions

- **The transcript is the handoff source.** The fresh instance reads the previous session's transcript itself and infers what's outstanding. Rejected alternatives: having the bloated session write a curated handoff note (costs tokens and cooperation from the old instance), repo-state-only guessing (misses conversational context), and Claude-native `--resume`/fork (restores the very context bloat being escaped).
- **Split alongside; the old pane is untouched.** The fresh instance opens in a split next to the stale pane, which stays until the user closes it. This mirrors the existing manual flow and keeps the old scrollback browsable during pickup.
- **Two chords instead of a guidance overlay.** The quick chord fires the stock prompt immediately. The guided chord launches the fresh instance with the same prompt pre-typed but unsent, so direction is appended in Claude's own input line. No new fly input surface; what will be sent is exactly what's visible.
- **Handoff panes are ordinary user panes.** They launch as a normal interactive `claude` in the user's default permission mode — unlike automation agent panes, which pass `--dangerously-skip-permissions`, are linked to a run, and are recursion-gated. None of that applies here.
- **Staleness detection is deferred.** fly proactively flagging "this pane is heavy — hand off?" is the named fast-follow, once the stock prompt has proven it reliably produces pickup.

---

## Requirements

**Trigger and placement**

- R1. Two leader-key actions operate on the focused pane: quick handoff and guided handoff.
- R2. Both actions appear in the hotkey cheat-sheet and command palette like every other binding.
- R3. Either action splits the focused pane and spawns the fresh instance alongside, in the old pane's current working directory, with focus moving to the new pane. The old pane is not closed, killed, or warned.

**Session resolution and prompt**

- R4. The stock handoff prompt names the exact transcript path of the pane's most recent tracked Claude session. Resolution works whether the old instance is still running or has exited.
- R5. The stock prompt directs the fresh instance to read the recent portion of the transcript — not the whole file — determine the outstanding work, and continue it.
- R6. Quick handoff sends the stock prompt immediately; the fresh instance starts working with no further input.
- R7. Guided handoff launches the fresh instance with the stock prompt pre-typed into its input but unsent. Nothing is sent until the user presses Enter; typed text appends after the pre-filled prefix.
- R8. If no previous session can be resolved for the pane, neither action spawns anything; the user sees a brief notice instead.

**Pane semantics**

- R9. Handoff panes launch as normal interactive sessions in the user's default permission mode.
- R10. Handoff panes are not automation panes: no run linkage, no completion deadline, no automation recursion gate, and full access to fly features from inside the pane.

---

## Key Flows

- F1. Quick handoff
  - **Trigger:** Leader chord on a stale agent pane.
  - **Steps:** fly resolves the pane's last session transcript; splits alongside in the same cwd; launches `claude` with the stock pickup prompt already sent.
  - **Outcome:** The fresh instance reports what's outstanding and continues; the user closes the old pane whenever they're done with it.
- F2. Guided handoff
  - **Trigger:** The alternate chord, on the same pane.
  - **Steps:** Same split and launch, but the stock prompt sits pre-typed and unsent in the fresh instance's input; the user appends direction (e.g., "run the tests before anything else") and presses Enter.
  - **Outcome:** The fresh instance starts with the stock pickup instruction plus the user's addition as one message.
- F3. No session to hand off
  - **Trigger:** Either chord on a pane with no tracked Claude session (e.g., a bare shell).
  - **Outcome:** No pane spawns; a brief notice says no previous session was found.

---

## Acceptance Examples

- AE1. **Covers R4, R6.** Given a pane whose Claude session went idle hours ago, when quick handoff fires, then a new split pane launches with a first message naming that session's transcript path, sent without any user input.
- AE2. **Covers R7.** Given guided handoff fired, when the fresh instance is ready, then the stock prompt is visible and editable in its input, nothing has been sent, and pressing Enter after typing an addition sends prefix plus addition together.
- AE3. **Covers R8.** Given a pane running a bare shell that never hosted a Claude session, when either chord fires, then no split is created and a brief notice explains why.

---

## Success Criteria

- Sit-down-to-working: the quick path costs two keystrokes — no palette typing, no manual prompt composition.
- Pickup quality: on a real handoff from a mostly-finished session, the fresh instance's opening move correctly identifies the outstanding work without the user restating context and without re-ingesting the full stale transcript.

---

## Scope Boundaries

Deferred for later:

- Staleness detection: a per-pane context-weight signal (transcript size is a cheap proxy) surfacing a "heavy — hand off?" affordance when a session crosses a threshold while idle. The click affordance for handoff rides with this.
- Any automated retirement of the old pane after handoff.

Rejected:

- The old session writing a curated handoff note before the new one starts.
- Using Claude Code's `--resume`/fork or `/compact` as the handoff vehicle.

---

## Outstanding Questions

Deferred to planning:

- Exact wording of the stock handoff prompt, within R5's constraints — worth a real-session iteration loop since pickup quality is the load-bearing risk.
- Reliable pre-typed-unsent injection for guided handoff: the fresh instance must be ready for input before the prefix is written, and the prefix must not self-submit.
- Which two chord keys to bind.

---

## Sources

- Per-pane session tracking already persists `session_id` and `session_cwd` across idle periods (`src-tauri/src/session/resume.rs`), and the transcript store derives session ids from `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, read-only (`src-tauri/src/session/transcript.rs`) — so R4's exact-transcript resolution needs no new capture machinery.
- The agent-pane spawn seam exists: `spawn_pane` takes an optional command vec (`src-tauri/src/stream/mod.rs`), and automations already launch `claude` panes with an argv prompt (`src/App.svelte`, `src/lib/automation-panes.ts`). Note automations pass `--dangerously-skip-permissions`; handoff must not (R9).
- New leader bindings propagate to the cheat-sheet and palette automatically from the shared bindings table (`src/lib/keymap.ts`, `src/lib/palette.ts`), satisfying R2 for free.
- The automations recursion registry only gates panes spawned with an automation run id, so handoff panes are ungated by construction (`src-tauri/src/automations/mod.rs`).
- Verified absent: no existing handoff, staleness, or transcript-size feature in the codebase; `src/lib/resume.ts` is same-pane crash-resume, a different concern.
