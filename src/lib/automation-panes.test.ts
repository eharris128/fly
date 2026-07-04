import { describe, it, expect } from "vitest";
import {
  findAutomationsWorkspace,
  buildAgentArgv,
  shouldAutoCloseRun,
} from "./automation-panes";
import type { Workspace } from "./workspaces";

function ws(id: string, role?: "automations"): Workspace {
  return { id, name: id, tabs: [], activeTabId: "", ...(role ? { role } : {}) };
}

describe("findAutomationsWorkspace (U6, R2)", () => {
  it("returns the id of the role-marked workspace", () => {
    const workspaces = [ws("ws-1"), ws("ws-2", "automations"), ws("ws-3")];
    expect(findAutomationsWorkspace(workspaces)).toBe("ws-2");
  });

  it("returns null when no workspace is marked", () => {
    expect(findAutomationsWorkspace([ws("ws-1"), ws("ws-2")])).toBeNull();
    expect(findAutomationsWorkspace([])).toBeNull();
  });

  it("returns the FIRST marked workspace when more than one exists (defensive)", () => {
    const workspaces = [ws("ws-1", "automations"), ws("ws-2", "automations")];
    expect(findAutomationsWorkspace(workspaces)).toBe("ws-1");
  });
});

describe("buildAgentArgv (U7, R11)", () => {
  it("appends --model / --effort / --fallback-model when set, prompt LAST", () => {
    expect(
      buildAgentArgv("summarize CI", {
        model: "opus",
        effort: "high",
        fallbackModel: "sonnet",
      }),
    ).toEqual([
      "claude",
      "--dangerously-skip-permissions",
      "--model",
      "opus",
      "--effort",
      "high",
      "--fallback-model",
      "sonnet",
      "summarize CI",
    ]);
  });

  it("omits each flag when its value is null", () => {
    expect(
      buildAgentArgv("do it", { model: "opus", effort: null, fallbackModel: null }),
    ).toEqual([
      "claude",
      "--dangerously-skip-permissions",
      "--model",
      "opus",
      "do it",
    ]);
  });

  it("with all flags null returns exactly today's argv (regression guard)", () => {
    expect(
      buildAgentArgv("hi", { model: null, effort: null, fallbackModel: null }),
    ).toEqual(["claude", "--dangerously-skip-permissions", "hi"]);
  });
});

describe("shouldAutoCloseRun (U8, R6/R7)", () => {
  it("auto-closes a succeeded run with no genuine raise", () => {
    expect(shouldAutoCloseRun("succeeded", false)).toBe(true);
  });

  it("keeps a succeeded run that still carries a genuine mid-run raise (R7)", () => {
    expect(shouldAutoCloseRun("succeeded", true)).toBe(false);
  });

  it("keeps a failed run regardless of raise (R7)", () => {
    expect(shouldAutoCloseRun("failed", false)).toBe(false);
    expect(shouldAutoCloseRun("failed", true)).toBe(false);
  });
});
