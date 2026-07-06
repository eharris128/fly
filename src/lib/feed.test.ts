import { describe, it, expect } from "vitest";
import { buildFeedPayload } from "./feed";
import type { HomeWorkspaceGroup, AgentRow } from "./home";

// Minimal AgentRow factory — only the fields buildFeedPayload reads matter.
function row(over: Partial<AgentRow> & Pick<AgentRow, "leafKey">): AgentRow {
  return {
    wsId: "ws-1",
    tabId: "tab-1",
    tabTitle: "fly",
    cwd: null,
    workingForMs: null,
    liveTaskCount: 0,
    needsAttention: false,
    reason: null,
    status: "idle",
    ...over,
  };
}

function model(rows: AgentRow[]): HomeWorkspaceGroup[] {
  return [
    {
      wsId: "ws-1",
      name: "home",
      tabs: [{ tabId: "tab-1", title: "fly", rows }],
    },
  ];
}

describe("buildFeedPayload", () => {
  it("maps a working agent row to an AgentEntry, carrying workspace/tab/cwd", () => {
    const { agents } = buildFeedPayload(
      model([
        row({
          leafKey: "leaf-1",
          cwd: "/home/evan/projects/fly",
          status: "working",
          workingForMs: 4200,
          num: 1,
        }),
      ]),
    );
    expect(agents).toHaveLength(1);
    expect(agents[0]).toMatchObject({
      leafKey: "leaf-1",
      workspace: "home",
      tab: "fly",
      cwd: "/home/evan/projects/fly",
      status: "working",
      workingForMs: 4200,
      num: 1,
      needsAttention: false,
    });
  });

  it("preserves needsAttention + reason for a waiting row", () => {
    const { agents } = buildFeedPayload(
      model([
        row({
          leafKey: "leaf-2",
          status: "waiting",
          needsAttention: true,
          reason: "permission",
        }),
      ]),
    );
    expect(agents[0].needsAttention).toBe(true);
    expect(agents[0].reason).toBe("permission");
  });

  it("normalizes an absent jump number to null", () => {
    const { agents } = buildFeedPayload(model([row({ leafKey: "leaf-3" })]));
    expect(agents[0].num).toBeNull();
  });

  it("always pushes lastReplyAt as null — the backend stamps it at emit", () => {
    // feed-agent-reply-io U1: the webview never knows reply times; a pushed
    // non-null value could go stale in the roster cache and desync from
    // /agents/{key}/output's repliedAt.
    const { agents } = buildFeedPayload(model([row({ leafKey: "leaf-4" })]));
    expect(agents[0].lastReplyAt).toBeNull();
  });

  it("returns an empty roster (not undefined) when no agents run", () => {
    expect(buildFeedPayload([])).toEqual({ agents: [] });
    expect(buildFeedPayload(model([]))).toEqual({ agents: [] });
  });
});
