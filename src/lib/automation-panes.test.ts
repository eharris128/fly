import { describe, it, expect } from "vitest";
import {
  findAutomationsWorkspace,
  buildAgentArgv,
  shouldAutoCloseRun,
  monitorCloseTarget,
} from "./automation-panes";
import type { Node } from "./layout";
import type { Workspace, Tab } from "./workspaces";

function ws(id: string, role?: "automations"): Workspace {
  return { id, name: id, tabs: [], activeTabId: "", ...(role ? { role } : {}) };
}

function leafNode(key: string): Node {
  return { kind: "leaf", key };
}

function tab(id: string, tree: Node): Tab {
  return { id, tree, focusedLeafKey: "", title: null };
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

describe("monitorCloseTarget (monitor-handoff U6, R13)", () => {
  it("resolves a sole-leaf pane to a whole-tab close across workspaces", () => {
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", leafNode("leaf-1"))] },
      { ...ws("ws-2"), tabs: [tab("tab-2", leafNode("leaf-2"))] },
    ];
    expect(monitorCloseTarget(workspaces, { 7: "leaf-2" }, 7)).toEqual({
      kind: "tab",
      tabId: "tab-2",
    });
  });

  it("returns null for a pane id absent from the reverse index", () => {
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", leafNode("leaf-1"))] },
    ];
    expect(monitorCloseTarget(workspaces, {}, 7)).toBeNull();
  });

  it("returns null when the mapped leaf no longer resolves (tab closed manually)", () => {
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", leafNode("leaf-1"))] },
    ];
    // Stale reverse-index entry: the pane exited and its tab is gone.
    expect(monitorCloseTarget(workspaces, { 7: "leaf-gone" }, 7)).toBeNull();
  });

  it("resolves a pane in a split to ONLY its own leaf (R13 — siblings survive)", () => {
    const split: Node = {
      kind: "split",
      key: "split-1",
      orientation: "horizontal",
      ratio: 0.5,
      first: leafNode("leaf-1"),
      second: leafNode("leaf-2"),
    };
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", split)] },
    ];
    // A whole-tab close would kill the sibling leaf's process without the
    // destructive confirm the user-initiated path gets — so a multi-leaf tab
    // resolves to a leaf-scoped close.
    expect(monitorCloseTarget(workspaces, { 7: "leaf-2" }, 7)).toEqual({
      kind: "leaf",
      tabId: "tab-1",
      leafKey: "leaf-2",
    });
  });
});
