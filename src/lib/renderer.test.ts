// KTD6 renderer eviction / perf-audit T4: the WebGL-attachment rule.
import { describe, expect, it } from "vitest";
import { wantsWebgl } from "./renderer";

describe("wantsWebgl", () => {
  it("auto attaches only while visible (the eviction policy)", () => {
    expect(wantsWebgl("auto", true, false)).toBe(true);
    expect(wantsWebgl("auto", false, false)).toBe(false);
  });

  it("webgl forces a context regardless of visibility", () => {
    expect(wantsWebgl("webgl", true, false)).toBe(true);
    expect(wantsWebgl("webgl", false, false)).toBe(true);
  });

  it("dom never attaches", () => {
    expect(wantsWebgl("dom", true, false)).toBe(false);
    expect(wantsWebgl("dom", false, false)).toBe(false);
  });

  it("a failed pane stays on the DOM renderer for good — no retry loop", () => {
    expect(wantsWebgl("auto", true, true)).toBe(false);
    expect(wantsWebgl("webgl", true, true)).toBe(false);
    expect(wantsWebgl("dom", true, true)).toBe(false);
  });
});
