import { describe, it, expect } from "vitest";
import {
  findAutomationsWorkspace,
  buildAgentArgv,
  shouldAutoCloseRun,
  tabForPane,
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

describe("tabForPane (monitor-handoff U6, R13)", () => {
  it("resolves a known pane to its enclosing tab across workspaces", () => {
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", leafNode("leaf-1"))] },
      { ...ws("ws-2"), tabs: [tab("tab-2", leafNode("leaf-2"))] },
    ];
    expect(tabForPane(workspaces, { 7: "leaf-2" }, 7)).toBe("tab-2");
  });

  it("returns null for a pane id absent from the reverse index", () => {
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", leafNode("leaf-1"))] },
    ];
    expect(tabForPane(workspaces, {}, 7)).toBeNull();
  });

  it("returns null when the mapped leaf no longer resolves (tab closed manually)", () => {
    const workspaces: Workspace[] = [
      { ...ws("ws-1"), tabs: [tab("tab-1", leafNode("leaf-1"))] },
    ];
    // Stale reverse-index entry: the pane exited and its tab is gone.
    expect(tabForPane(workspaces, { 7: "leaf-gone" }, 7)).toBeNull();
  });

  it("resolves a pane in a split to the WHOLE enclosing tab (R13 closes the tab)", () => {
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
    // Closing this tab also closes the sibling leaf — the documented R13 edge.
    expect(tabForPane(workspaces, { 7: "leaf-2" }, 7)).toBe("tab-1");
  });
});
