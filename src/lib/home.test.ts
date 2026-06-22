import { describe, it, expect } from "vitest";
import { buildHomeModel, formatDuration, agentCount } from "./home";
import type { Tab, Workspace } from "./workspaces";
import type { Node } from "./layout";
import type { PaneActivity } from "../ipc";

const leaf = (key: string): Node => ({ kind: "leaf", key });
const split = (key: string, first: Node, second: Node): Node => ({
  kind: "split",
  key,
  orientation: "horizontal",
  ratio: 0.5,
  first,
  second,
});

function tab(id: string, tree: Node, focusedLeafKey: string, title: string | null = null): Tab {
  return { id, tree, focusedLeafKey, title };
}
function ws(id: string, name: string, tabs: Tab[]): Workspace {
  return { id, name, tabs, activeTabId: tabs[0]?.id ?? "" };
}
function agent(isAgent: boolean, workingForMs: number | null = null): PaneActivity {
  return { isAgent, workingForMs, lastOutputAgoMs: workingForMs };
}

describe("buildHomeModel", () => {
  it("groups agent leaves under their workspace and tab", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [
        tab("tab-1", leaf("leaf-1"), "leaf-1"),
        tab("tab-2", leaf("leaf-2"), "leaf-2"),
      ]),
    ];
    const model = buildHomeModel(
      workspaces,
      { "leaf-1": agent(true, 5000), "leaf-2": agent(false) },
      { "leaf-1": "/home/u/proj" },
      {},
    );
    expect(model).toHaveLength(1);
    expect(model[0].wsId).toBe("ws-1");
    expect(model[0].name).toBe("Alpha");
    // tab-2 (non-agent) is dropped entirely.
    expect(model[0].tabs).toHaveLength(1);
    expect(model[0].tabs[0].tabId).toBe("tab-1");
    expect(model[0].tabs[0].title).toBe("proj");
    expect(model[0].tabs[0].rows).toHaveLength(1);
    const row = model[0].tabs[0].rows[0];
    expect(row).toMatchObject({
      leafKey: "leaf-1",
      cwd: "/home/u/proj",
      workingForMs: 5000,
      needsAttention: false,
      status: "working",
    });
  });

  it("excludes non-agent leaves and omits empty tabs/workspaces", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [tab("tab-1", leaf("leaf-1"), "leaf-1")]),
      ws("ws-2", "Beta", [tab("tab-2", leaf("leaf-2"), "leaf-2")]),
    ];
    const model = buildHomeModel(
      workspaces,
      { "leaf-1": agent(false), "leaf-2": agent(false) },
      {},
      {},
    );
    expect(model).toEqual([]); // no agents anywhere → empty (R7)
    expect(agentCount(model)).toBe(0);
  });

  it("applies the status precedence: attention > working > idle", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [
        tab("tab-1", split("s-1", leaf("a"), split("s-2", leaf("b"), leaf("c"))), "a"),
      ]),
    ];
    const model = buildHomeModel(
      workspaces,
      {
        a: agent(true, 9000), // attention raised → waiting despite a stretch
        b: agent(true, 3000), // idle attention + stretch → working
        c: agent(true, null), // idle attention + no stretch → idle
      },
      {},
      { a: "raised" },
    );
    const rows = model[0].tabs[0].rows;
    expect(rows.map((r) => [r.leafKey, r.status])).toEqual([
      ["a", "waiting"],
      ["b", "working"],
      ["c", "idle"],
    ]);
    // attention wins even though `a` has a non-null stretch.
    expect(rows[0].needsAttention).toBe(true);
    expect(rows[0].workingForMs).toBe(9000);
  });

  it("treats acknowledged the same as raised for needs-attention", () => {
    const workspaces = [ws("ws-1", "Alpha", [tab("tab-1", leaf("leaf-1"), "leaf-1")])];
    const model = buildHomeModel(
      workspaces,
      { "leaf-1": agent(true, 4000) },
      {},
      { "leaf-1": "acknowledged" },
    );
    expect(model[0].tabs[0].rows[0].status).toBe("waiting");
  });

  it("carries workingForMs through, null when idle", () => {
    const workspaces = [ws("ws-1", "Alpha", [tab("tab-1", leaf("leaf-1"), "leaf-1")])];
    const idle = buildHomeModel(workspaces, { "leaf-1": agent(true, null) }, {}, {});
    expect(idle[0].tabs[0].rows[0].workingForMs).toBeNull();
    expect(idle[0].tabs[0].rows[0].status).toBe("idle");
  });

  it("returns [] for empty inputs", () => {
    expect(buildHomeModel([], {}, {}, {})).toEqual([]);
  });
});

describe("formatDuration", () => {
  it("formats seconds, minutes, and hours", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(999)).toBe("0s");
    expect(formatDuration(1000)).toBe("1s");
    expect(formatDuration(45_000)).toBe("45s");
    expect(formatDuration(59_000)).toBe("59s");
    expect(formatDuration(60_000)).toBe("1m"); // boundary at 60s
    expect(formatDuration(3_599_000)).toBe("59m");
    expect(formatDuration(3_600_000)).toBe("1h"); // boundary at 3600s
    expect(formatDuration(3_660_000)).toBe("1h 1m");
    expect(formatDuration(7_500_000)).toBe("2h 5m");
  });

  it("clamps negative input to 0s", () => {
    expect(formatDuration(-5000)).toBe("0s");
  });
});
