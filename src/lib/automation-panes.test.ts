import { describe, it, expect } from "vitest";
import { resolveTargetWorkspace } from "./automation-panes";
import type { Workspace } from "./workspaces";

function ws(id: string): Workspace {
  return { id, name: id, tabs: [], activeTabId: "" };
}

describe("resolveTargetWorkspace", () => {
  it("places into the origin workspace when it still exists (R9)", () => {
    const workspaces = [ws("ws-1"), ws("ws-2"), ws("ws-3")];
    expect(resolveTargetWorkspace(workspaces, "ws-2")).toBe("ws-2");
  });

  it("falls back to the first workspace when the origin is gone (R9)", () => {
    // A hint from a prior launch / closed workspace never matches — land in the
    // first workspace rather than dropping the run.
    const workspaces = [ws("ws-5"), ws("ws-6")];
    expect(resolveTargetWorkspace(workspaces, "ws-stale")).toBe("ws-5");
  });

  it("returns null only when there are no workspaces to place into", () => {
    expect(resolveTargetWorkspace([], "ws-1")).toBeNull();
  });
});
