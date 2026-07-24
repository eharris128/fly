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

  // phone-screenshot-drop U4: paneId is identity, not addressing — the drop
  // route echoes it back to detect a session replaced in the same leaf slot.
  describe("paneId", () => {
    it("carries the pane id from the supplied leaf→pane map", () => {
      const { agents } = buildFeedPayload(
        model([row({ leafKey: "leaf-1" }), row({ leafKey: "leaf-2" })]),
        { "leaf-1": 11, "leaf-2": 22 },
      );
      expect(agents.map((a) => a.paneId)).toEqual([11, 22]);
    });

    it("publishes a leaf with no pane id as paneId: null rather than dropping it", () => {
      // Dropping the entry would make a starting agent vanish from the roster
      // entirely; null keeps it visible but marks it not-yet-targetable.
      const { agents } = buildFeedPayload(
        model([row({ leafKey: "leaf-1" }), row({ leafKey: "starting" })]),
        { "leaf-1": 11 },
      );
      expect(agents).toHaveLength(2);
      expect(agents[1]).toMatchObject({ leafKey: "starting", paneId: null });
    });

    it("defaults every paneId to null when no map is supplied", () => {
      const { agents } = buildFeedPayload(model([row({ leafKey: "leaf-1" })]));
      expect(agents[0].paneId).toBeNull();
    });

    it("ignores map entries for leaves that are not in the roster", () => {
      const { agents } = buildFeedPayload(model([row({ leafKey: "leaf-1" })]), {
        "leaf-1": 11,
        "leaf-gone": 99,
      });
      expect(agents).toHaveLength(1);
      expect(agents[0].paneId).toBe(11);
    });
  });

  // R4: two sessions in the same directory must still be tellable apart from a
  // phone, which is why the page renders workspace/tab/num and not just cwd.
  it("distinguishes two agents sharing a cwd by workspace, tab, and num", () => {
    const groups: HomeWorkspaceGroup[] = [
      {
        wsId: "ws-1",
        name: "home",
        tabs: [
          {
            tabId: "tab-1",
            title: "api",
            rows: [row({ leafKey: "leaf-1", cwd: "/srv/app", num: 1 })],
          },
        ],
      },
      {
        wsId: "ws-2",
        name: "review",
        tabs: [
          {
            tabId: "tab-2",
            title: "web",
            rows: [row({ leafKey: "leaf-2", cwd: "/srv/app", num: 2 })],
          },
        ],
      },
    ];
    const { agents } = buildFeedPayload(groups, { "leaf-1": 11, "leaf-2": 22 });
    expect(agents[0].cwd).toBe(agents[1].cwd);
    expect(agents[0].workspace).not.toBe(agents[1].workspace);
    expect(agents[0].tab).not.toBe(agents[1].tab);
    expect(agents[0].num).not.toBe(agents[1].num);
  });

  // A drift guard: adding paneId must not have perturbed any other field.
  it("emits exactly the expected field set for one row", () => {
    const { agents } = buildFeedPayload(
      model([
        row({
          leafKey: "leaf-1",
          cwd: "/srv/app",
          status: "working",
          workingForMs: 1200,
          liveTaskCount: 3,
          needsAttention: true,
          reason: "question",
          num: 4,
        }),
      ]),
      { "leaf-1": 7 },
    );
    expect(agents[0]).toEqual({
      leafKey: "leaf-1",
      workspace: "home",
      tab: "fly",
      cwd: "/srv/app",
      status: "working",
      needsAttention: true,
      reason: "question",
      workingForMs: 1200,
      liveTaskCount: 3,
      num: 4,
      lastReplyAt: null,
      questionPendingAt: null,
      paneId: 7,
    });
  });
});
