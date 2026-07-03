// Session pick-list view-model (fix-session-pane-attribution U6;
// docs/plans/2026-07-03-001-fix-session-pane-attribution-plan.md, R6-R10).
// A pure data module like `home.ts` / `nudge.ts` — no DOM, no Svelte, no ipc
// calls — so the picker's rows, routing, and provenance wording are unit-tested
// without an app. App.svelte owns the overlay + the awaited promise
// (`resumeOffer` pattern) and feeds candidates from `listHandoffCandidates`.

import type { HandoffCandidate, SessionSource } from "../ipc";
import { relativeTime } from "./notifications";

/** One rendered picker row: recognizable by its recent-turn snippet and
 *  last-activity time (R7); the index maps back to the candidate. */
export interface SessionPickerRow {
  /** First 8 chars of the session id — enough to tell siblings apart. */
  shortId: string;
  /** Recent-turn excerpt, or the explicit fallback when no turn had text. */
  snippet: string;
  /** Relative last-activity label ("3h ago"). */
  when: string;
}

/** The picker row's fallback when a qualifying transcript has real turns but
 *  none with extractable text (e.g. a tool-use-only session). */
export const NO_SNIPPET_FALLBACK = "(no readable turn text)";

/** The human-facing short form of a session id — enough to tell same-cwd
 *  siblings apart in a row or a notice. */
export function shortSessionId(id: string): string {
  return id.slice(0, 8);
}

/**
 * Order candidates most-recent-activity first (R7). The backend already
 * orders by last real turn, but the sort is re-applied at the seam so the
 * view can never drift from the contract it renders. App stores THIS array
 * and renders rows from it, so a row index always maps back to its candidate.
 */
export function sortCandidates(
  candidates: HandoffCandidate[],
): HandoffCandidate[] {
  return [...candidates].sort((a, b) => b.lastTurnMs - a.lastTurnMs);
}

/** Already-sorted candidates → rendered rows, index-aligned 1:1. */
export function candidatesToRows(
  candidates: HandoffCandidate[],
  now: number,
): SessionPickerRow[] {
  return candidates.map((c) => ({
    shortId: shortSessionId(c.sessionId),
    snippet: c.snippet ?? NO_SNIPPET_FALLBACK,
    when: relativeTime(c.lastTurnMs, now),
  }));
}

/** Clamp a selection index into `[0, len)` (0 for an empty list) — clearing or
 *  refreshing the list must never strand the highlight out of range. */
export function clampIndex(i: number, len: number): number {
  if (len <= 0) return 0;
  return Math.min(Math.max(i, 0), len - 1);
}

/**
 * How an ambiguous launch routes (R6/R9/R11): no candidates → the existing
 * "no previous session" notice, never an empty picker; exactly one → proceed
 * zero-prompt (the cwd is unambiguous); several → the pick-list. `forceList`
 * (a divergence re-pick or the U8 force re-pick) always shows the list — the
 * user must confirm even a single candidate there, because the whole point is
 * re-examining a suspect binding.
 */
export type PickerPlan = "notice" | "auto" | "list";

export function pickerPlan(candidateCount: number, forceList: boolean): PickerPlan {
  if (candidateCount === 0) return "notice";
  if (candidateCount === 1 && !forceList) return "auto";
  return "list";
}

/**
 * The two ambiguity-routing predicates the handoff flow branches on, lifted out
 * of App.svelte so the AE6 guarantee can never drift from inline booleans (R14,
 * KTD2). Both take the minimal shape a resolved target exposes, so they test
 * without a full `HandoffTarget`.
 *
 * `takesPrecisePath`: the zero-prompt AE1 branch — a target resolved with
 * confidence (present and NOT divergence-pending) hands off straight away. A
 * null target (nothing resolved) routes to the pick path instead.
 */
export function takesPrecisePath(
  target: { divergencePending: boolean } | null,
): boolean {
  return target !== null && !target.divergencePending;
}

/**
 * `shouldForceSessionPick`: when the pick-list must show even a single candidate
 * (`pickerPlan`'s `forceList`). True for an explicit force re-pick (U8) or a
 * divergence-pending target (KTD2) — the AE6 guarantee that a divergent `Hook`
 * can't silently redirect a single-candidate launch. Auto-proceeding there would
 * defeat the whole point of re-examining a suspect binding.
 */
export function shouldForceSessionPick(
  target: { divergencePending: boolean } | null,
  forcePick: boolean,
): boolean {
  return forcePick || target?.divergencePending === true;
}

/**
 * Human wording for a resolved target's provenance (KTD4: shown at handoff so
 * a remembered rebind — or a lingering poll guess — is never invisible).
 */
export function provenanceLabel(source: SessionSource): string {
  switch (source) {
    case "pick":
      return "your remembered pick";
    case "hook":
      return "captured at session start";
    case "poll":
      return "most recent in folder";
  }
}
