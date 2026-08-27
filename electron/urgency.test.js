// Tests for the pure urgency rule (2026-08-27-001 KTD4).
import { describe, it, expect } from "vitest";
import { shouldFlash } from "./urgency.js";

const raised = { paneId: 1, state: "raised", reason: "permission", tier: "hook" };

describe("shouldFlash", () => {
  it("flashes for a raise while the window is unfocused", () => {
    expect(shouldFlash("pane://attention", raised, false)).toBe(true);
  });
  it("never flashes a focused window — the user is looking", () => {
    expect(shouldFlash("pane://attention", raised, true)).toBe(false);
  });
  it("ignores non-raise states (acknowledge/idle transitions)", () => {
    for (const state of ["acknowledged", "idle"]) {
      expect(shouldFlash("pane://attention", { ...raised, state }, false)).toBe(false);
    }
  });
  it("ignores every other event and a missing payload", () => {
    expect(shouldFlash("pane://exit", raised, false)).toBe(false);
    expect(shouldFlash("automation://changed", "a1", false)).toBe(false);
    expect(shouldFlash("pane://attention", undefined, false)).toBe(false);
  });
});
