// The pure half of renderer-crash recovery (recovery.js). main.js wires these
// to Electron's events; the decisions themselves are pinned here.
import { describe, it, expect } from "vitest";
import { ReloadBudget, canDeliver, closePlan, needsRecovery } from "./recovery.js";

describe("ReloadBudget", () => {
  it("allows up to max reloads inside the window, then refuses", () => {
    const b = new ReloadBudget({ max: 3, windowMs: 60_000 });
    expect(b.note(0)).toBe(true);
    expect(b.note(1_000)).toBe(true);
    expect(b.note(2_000)).toBe(true);
    expect(b.note(3_000)).toBe(false); // the storm guard
    expect(b.note(30_000)).toBe(false);
  });

  it("forgets attempts older than the window", () => {
    const b = new ReloadBudget({ max: 2, windowMs: 10_000 });
    expect(b.note(0)).toBe(true);
    expect(b.note(1_000)).toBe(true);
    expect(b.note(5_000)).toBe(false);
    expect(b.note(10_001)).toBe(true); // the first attempt aged out
    expect(b.note(10_002)).toBe(false); // 1_000 and 10_001 still inside
  });

  it("reset forgives (manual reload from the crash page)", () => {
    const b = new ReloadBudget({ max: 1, windowMs: 60_000 });
    expect(b.note(0)).toBe(true);
    expect(b.note(1)).toBe(false);
    b.reset();
    expect(b.note(2)).toBe(true);
  });
});

describe("canDeliver", () => {
  it("refuses a crashed frame even though the window is not destroyed", () => {
    // The incident shape: win.isDestroyed() false, every send throwing.
    expect(canDeliver({ destroyed: false, crashed: true })).toBe(false);
  });
  it("refuses a destroyed webContents", () => {
    expect(canDeliver({ destroyed: true, crashed: false })).toBe(false);
  });
  it("delivers to a live frame", () => {
    expect(canDeliver({ destroyed: false, crashed: false })).toBe(true);
  });
});

describe("closePlan", () => {
  const live = { crashed: false, hung: false, loaded: true, onErrorPage: false };
  it("asks a live renderer (the busy-agents confirm flow stays)", () => {
    expect(closePlan(live)).toBe("ask");
  });
  it("destroys when no verdict can ever arrive", () => {
    expect(closePlan({ ...live, crashed: true })).toBe("destroy");
    expect(closePlan({ ...live, hung: true })).toBe("destroy");
    expect(closePlan({ ...live, loaded: false })).toBe("destroy");
    expect(closePlan({ ...live, onErrorPage: true })).toBe("destroy");
  });
});

describe("needsRecovery", () => {
  it("recovers every real death", () => {
    for (const reason of ["crashed", "oom", "killed", "abnormal-exit", "launch-failed"]) {
      expect(needsRecovery({ reason, loading: false })).toBe(true);
      expect(needsRecovery({ reason, loading: true })).toBe(true);
    }
  });
  it("recovers a clean exit that left nothing behind", () => {
    expect(needsRecovery({ reason: "clean-exit", loading: false })).toBe(true);
  });
  it("leaves a navigation process swap alone", () => {
    expect(needsRecovery({ reason: "clean-exit", loading: true })).toBe(false);
  });
});
