// Pure decision logic for the attention-triage nudge (U3). Framework-free — no
// DOM, no Svelte — like `home.ts` / `keymap.ts`, so it is unit-tested with an
// injected `now`. App (U5/U6) computes the live signals and feeds them here; the
// rules for "should the nudge show" and "what does a key do" live in one place.
//
// The nudge is the loop's "move along" prompt: once you've handled the focused
// agent and it no longer needs you, a permeable overlay appears and Tab rotates
// you onward (R9–R15). Detecting "resumed working" is the subtle part — the
// backend attention machine has no output-driven transition (KTD1), so that
// signal comes from the focused-pane `pane_activity` poll's `workingForMs`, not
// from `pane://attention`.

import type { AttentionState, AttentionReason } from "../ipc";

/** What a key does while the nudge overlay is showing (R12–R14, KTD2). */
export type NudgeKeyAction = "rotate" | "dismiss-stay" | "dismiss-passthrough";

/**
 * Map a `KeyboardEvent.key` to the nudge's action: Tab rotates to the next agent
 * (or the dashboard), Escape dismisses and keeps you here, and every other key
 * dismisses and passes through to the focused pane with no keystroke lost (R14).
 * Matched on the literal key — never on `shiftKey` — mirroring keymap.ts (KTD2).
 */
export function keyAction(key: string): NudgeKeyAction {
  if (key === "Tab") return "rotate";
  if (key === "Escape") return "dismiss-stay";
  return "dismiss-passthrough";
}

/**
 * Transition of the focused agent's work stretch between two activity polls.
 * `workingForMs` is non-null while the agent is busy; the only transitions that
 * matter are the null↔non-null edges (KTD1). The caller latches "moved on" when
 * this is anything but `none` after engagement.
 */
export type BusyTransition = "became-busy" | "became-idle" | "none";
export function deriveBusyIdle(
  prevWorkingForMs: number | null | undefined,
  currWorkingForMs: number | null,
): BusyTransition {
  const prevBusy = prevWorkingForMs != null;
  const currBusy = currWorkingForMs != null;
  if (!prevBusy && currBusy) return "became-busy";
  if (prevBusy && !currBusy) return "became-idle";
  return "none";
}

/** Elapsed ms since the user last typed (R9's keystroke-only idle clock). */
export function userIdleMs(now: number, lastActivityAt: number): number {
  return Math.max(0, now - lastActivityAt);
}

/**
 * Whether the focused agent is currently waiting on YOU — raised with a question
 * or permission prompt. While this holds, the nudge stays silent (R10): you
 * answer it in place rather than being rotated away. A `finished` raise is NOT
 * "needs you" — it's done, which is exactly when the nudge should fire.
 */
export function needsYouNow(
  attention: AttentionState,
  reason: AttentionReason | null,
): boolean {
  return (
    attention === "raised" && (reason === "question" || reason === "permission")
  );
}

export interface NudgeInput {
  /** Have you engaged this focused agent this episode (viewed / typed to it)? */
  engaged: boolean;
  /** Effective attention state of the focused leaf. */
  attention: AttentionState;
  /** Last-raise reason of the focused leaf — the finished-vs-question
   *  discriminator the attention state alone can't carry (KTD1). */
  reason: AttentionReason | null;
  /** Since you engaged, the agent resumed working or finished a stretch — i.e.
   *  it stopped needing you. Latched by the caller from {@link deriveBusyIdle}
   *  (or a `finished` raise); avoids nudging when nothing actually happened. */
  movedOn: boolean;
  /** ms since you last typed (see {@link userIdleMs}). */
  userIdleMs: number;
  /** Configured idle delay N before the nudge fires (R16). */
  nudgeIdleMs: number;
}

/**
 * Decide whether the nudge overlay should show for the focused agent (R9–R11).
 * Show when: you've engaged it, it has moved on (resumed working or finished),
 * you've been idle at least N, and it is not currently re-raised needing an
 * answer. The re-raise check wins over `movedOn` so a follow-up question always
 * suppresses the nudge (R10/AE3).
 */
export function shouldShowNudge(input: NudgeInput): boolean {
  if (!input.engaged) return false;
  if (needsYouNow(input.attention, input.reason)) return false;
  if (!input.movedOn) return false;
  return input.userIdleMs >= input.nudgeIdleMs;
}
