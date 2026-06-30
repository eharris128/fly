import { describe, it, expect } from "vitest";
import {
  shouldShowNudge,
  deriveBusyIdle,
  userIdleMs,
  needsYouNow,
  keyAction,
  type NudgeInput,
} from "./nudge";

// A baseline "nudge should fire" input: engaged, moved on, not waiting on you,
// idle past N. Each test overrides only the field under scrutiny.
function input(over: Partial<NudgeInput> = {}): NudgeInput {
  return {
    engaged: true,
    sawRaise: true,
    attention: "idle",
    reason: null,
    movedOn: true,
    userIdleMs: 5000,
    nudgeIdleMs: 4000,
    ...over,
  };
}

describe("shouldShowNudge", () => {
  it("Covers AE1: shows when the agent resumed working and you've been idle ≥ N", () => {
    // moved on via a became-busy transition; attention idle/acknowledged.
    expect(shouldShowNudge(input({ movedOn: true, attention: "acknowledged" }))).toBe(true);
  });

  it("Covers AE2: shows when the agent finished/went idle (not stranded on a done agent)", () => {
    expect(shouldShowNudge(input({ movedOn: true, attention: "idle", reason: "finished" }))).toBe(
      true,
    );
  });

  it("Covers AE3: never shows while the agent re-raises with a question", () => {
    expect(
      shouldShowNudge(input({ attention: "raised", reason: "question" })),
    ).toBe(false);
  });

  it("Covers AE3: never shows while the agent re-raises for permission", () => {
    expect(
      shouldShowNudge(input({ attention: "raised", reason: "permission" })),
    ).toBe(false);
  });

  it("a finished raise is not 'needs you' — it still nudges", () => {
    expect(
      shouldShowNudge(input({ attention: "raised", reason: "finished" })),
    ).toBe(true);
  });

  it("does not show until you've been idle for N", () => {
    expect(shouldShowNudge(input({ userIdleMs: 3999, nudgeIdleMs: 4000 }))).toBe(false);
  });

  it("shows exactly at the N boundary (inclusive)", () => {
    expect(shouldShowNudge(input({ userIdleMs: 4000, nudgeIdleMs: 4000 }))).toBe(true);
  });

  it("does not show before you've engaged the focused agent", () => {
    expect(shouldShowNudge(input({ engaged: false }))).toBe(false);
  });

  it("does not show for an agent that never raised this episode (you launched it, didn't triage it)", () => {
    // Type `claude` → engaged; its startup work burst → movedOn; you idle past N.
    // With no raise to handle, the nudge must stay silent (R9 presupposes a raise).
    expect(
      shouldShowNudge(input({ sawRaise: false, engaged: true, movedOn: true })),
    ).toBe(false);
  });

  it("does not show until the agent has moved on (resumed working or finished)", () => {
    expect(shouldShowNudge(input({ movedOn: false }))).toBe(false);
  });
});

describe("needsYouNow", () => {
  it("is true for a question/permission raise", () => {
    expect(needsYouNow("raised", "question")).toBe(true);
    expect(needsYouNow("raised", "permission")).toBe(true);
  });
  it("counts a focused pane (acknowledged) awaiting an answer", () => {
    // A raise on the visible focused pane collapses to acknowledged; reason is
    // the discriminator (KTD1), so it must still suppress the nudge.
    expect(needsYouNow("acknowledged", "question")).toBe(true);
    expect(needsYouNow("acknowledged", "permission")).toBe(true);
  });
  it("is false for a finished/error raise (done, not waiting on you)", () => {
    expect(needsYouNow("raised", "finished")).toBe(false);
    expect(needsYouNow("acknowledged", "finished")).toBe(false);
    expect(needsYouNow("raised", "error")).toBe(false);
  });
  it("is false once cleared to idle (you replied — reason is null)", () => {
    expect(needsYouNow("idle", "question")).toBe(false);
    expect(needsYouNow("idle", null)).toBe(false);
  });
});

describe("deriveBusyIdle", () => {
  it("null → non-null is became-busy (resumed working)", () => {
    expect(deriveBusyIdle(null, 1200)).toBe("became-busy");
    expect(deriveBusyIdle(undefined, 1)).toBe("became-busy");
  });
  it("non-null → null is became-idle (finished a stretch)", () => {
    expect(deriveBusyIdle(3000, null)).toBe("became-idle");
  });
  it("no edge → none", () => {
    expect(deriveBusyIdle(null, null)).toBe("none");
    expect(deriveBusyIdle(2000, 3000)).toBe("none");
    expect(deriveBusyIdle(undefined, null)).toBe("none");
  });
});

describe("userIdleMs", () => {
  it("returns elapsed ms since the last keystroke", () => {
    expect(userIdleMs(5000, 1000)).toBe(4000);
  });
  it("clamps to 0 for a future/equal stamp (no negative idle)", () => {
    expect(userIdleMs(1000, 1000)).toBe(0);
    expect(userIdleMs(1000, 2000)).toBe(0);
  });
});

describe("keyAction", () => {
  it("Tab rotates, Escape dismisses-and-stays", () => {
    expect(keyAction("Tab")).toBe("rotate");
    expect(keyAction("Escape")).toBe("dismiss-stay");
  });
  it("every other key dismisses and passes through (R14)", () => {
    expect(keyAction("a")).toBe("dismiss-passthrough");
    expect(keyAction("Enter")).toBe("dismiss-passthrough");
    expect(keyAction("c")).toBe("dismiss-passthrough"); // e.g. Ctrl-C's key
    expect(keyAction(" ")).toBe("dismiss-passthrough");
  });
});
