import { describe, it, expect, beforeEach } from "vitest";
import { newLeaf, splitLeaf, leaves, resetKeys, type Node } from "./layout";
import {
  basename,
  tabDisplayTitle,
  findTab,
  closeTabIn,
  deleteWorkspaceFrom,
  persistedTabs,
  scrollbackLeafKeys,
  flattenRaised,
  attentionPriority,
  sortByAttentionPriority,
  attentionKind,
  combineAttentionKinds,
  rollupAttentionKind,
  unreadCountForLeaves,
  reorderWorkspaces,
  insertionIndex,
  sourceLeafForNewTab,
  type Tab,
  type Workspace,
} from "./workspaces";

let tabSeq = 0;
let wsSeq = 0;
function makeTab(title: string | null = null, tree?: Node): Tab {
  const t = tree ?? newLeaf();
  return {
    id: `tab-${++tabSeq}`,
    tree: t,
    focusedLeafKey: t.kind === "leaf" ? t.key : "",
    title,
  };
}
function makeWorkspace(name: string, tabs: Tab[]): Workspace {
  return { id: `ws-${++wsSeq}`, name, tabs, activeTabId: tabs[0]?.id ?? "" };
}

beforeEach(() => {
  resetKeys();
  tabSeq = 0;
  wsSeq = 0;
});

describe("basename", () => {
  it("returns the last path segment", () => {
    expect(basename("/home/evan/projects/fly")).toBe("fly");
    expect(basename("/home/evan")).toBe("evan");
  });
  it("ignores trailing slashes and handles root", () => {
    expect(basename("/home/evan/")).toBe("evan");
    expect(basename("/")).toBe("/");
    expect(basename("//")).toBe("/");
  });
});

describe("tabDisplayTitle", () => {
  it("prefers a manual title, trimmed", () => {
    const tab = makeTab("  My Agent  ");
    expect(tabDisplayTitle(tab, {})).toBe("My Agent");
  });
  it("auto-names from the focused leaf cwd basename", () => {
    const leaf = newLeaf();
    const tab = makeTab(null, leaf);
    expect(tabDisplayTitle(tab, { [leaf.key]: "/home/evan/projects/fly" })).toBe(
      "fly",
    );
  });
  it("prefers the focused leaf over other leaves", () => {
    const res = splitLeaf(newLeaf(), "leaf-1", "horizontal")!;
    const tab: Tab = {
      id: "tab-x",
      tree: res.tree,
      focusedLeafKey: res.added.key,
      title: null,
    };
    const cwds = { "leaf-1": "/a/first", [res.added.key]: "/b/second" };
    expect(tabDisplayTitle(tab, cwds)).toBe("second");
  });
  it("falls back to the first known cwd, then to 'shell'", () => {
    const res = splitLeaf(newLeaf(), "leaf-1", "horizontal")!;
    const tab: Tab = {
      id: "tab-x",
      tree: res.tree,
      focusedLeafKey: "missing", // focused leaf has no cwd
      title: null,
    };
    expect(tabDisplayTitle(tab, { "leaf-1": "/x/alpha" })).toBe("alpha");
    expect(tabDisplayTitle(makeTab(null), {})).toBe("shell");
  });
});

describe("findTab", () => {
  it("locates a tab and its owning workspace", () => {
    const a = makeTab();
    const b = makeTab();
    const wsA = makeWorkspace("a", [a]);
    const wsB = makeWorkspace("b", [b]);
    expect(findTab([wsA, wsB], b.id)?.ws.id).toBe(wsB.id);
    expect(findTab([wsA, wsB], "nope")).toBeNull();
  });
});

describe("closeTabIn", () => {
  it("removes a tab and moves the active id to the left neighbour", () => {
    const t1 = makeTab();
    const t2 = makeTab();
    const t3 = makeTab();
    const ws = { ...makeWorkspace("w", [t1, t2, t3]), activeTabId: t2.id };
    const [out] = closeTabIn([ws], t2.id, () => makeTab());
    expect(out.tabs.map((t) => t.id)).toEqual([t1.id, t3.id]);
    expect(out.activeTabId).toBe(t1.id);
  });
  it("leaves activeTabId alone when a non-active tab closes", () => {
    const t1 = makeTab();
    const t2 = makeTab();
    const ws = { ...makeWorkspace("w", [t1, t2]), activeTabId: t2.id };
    const [out] = closeTabIn([ws], t1.id, () => makeTab());
    expect(out.activeTabId).toBe(t2.id);
  });
  it("inserts a fresh tab when the last tab in a workspace closes", () => {
    const only = makeTab();
    const ws = makeWorkspace("w", [only]);
    const fresh = makeTab();
    const [out] = closeTabIn([ws], only.id, () => fresh);
    expect(out.tabs).toEqual([fresh]);
    expect(out.activeTabId).toBe(fresh.id);
  });
  it("only touches the workspace that owns the tab", () => {
    const t1 = makeTab();
    const t2 = makeTab();
    const wsA = makeWorkspace("a", [t1]);
    const wsB = makeWorkspace("b", [t2]);
    const out = closeTabIn([wsA, wsB], t2.id, () => makeTab());
    expect(out[0]).toBe(wsA); // untouched reference
  });
  it("closes an ephemeral tab exactly like any other tab (U11)", () => {
    const t1 = makeTab();
    const eph: Tab = { ...makeTab(), ephemeral: true };
    const t3 = makeTab();
    const ws = { ...makeWorkspace("w", [t1, eph, t3]), activeTabId: eph.id };
    const [out] = closeTabIn([ws], eph.id, () => makeTab());
    expect(out.tabs.map((t) => t.id)).toEqual([t1.id, t3.id]);
    expect(out.activeTabId).toBe(t1.id); // left neighbour, same as a normal close
  });
  it("replaces a closing last ephemeral tab with a fresh one, like any last-tab close (U11)", () => {
    const eph: Tab = { ...makeTab(), ephemeral: true };
    const ws = makeWorkspace("w", [eph]);
    const fresh = makeTab();
    const [out] = closeTabIn([ws], eph.id, () => fresh);
    expect(out.tabs).toEqual([fresh]);
    expect(out.activeTabId).toBe(fresh.id);
  });
});

describe("persistedTabs", () => {
  it("filters ephemeral tabs out while keeping siblings in order (R12)", () => {
    const t1 = makeTab();
    const eph: Tab = { ...makeTab(), ephemeral: true };
    const t3 = makeTab();
    expect(persistedTabs([t1, eph, t3])).toEqual([t1, t3]);
  });
  it("keeps every tab when no ephemeral flag is present anywhere (KTD-G default)", () => {
    const tabs = [makeTab(), makeTab()];
    expect(persistedTabs(tabs)).toEqual(tabs);
  });
});

describe("scrollbackLeafKeys", () => {
  it("excludes an ephemeral tab's leaves from the scrollback-save candidate list (R12)", () => {
    const split = splitLeaf(newLeaf(), "leaf-1", "horizontal")!; // two-leaf tab
    const normal = makeTab(null, split.tree);
    const eph: Tab = { ...makeTab(), ephemeral: true };
    const other = makeTab();
    const wsA = makeWorkspace("a", [normal, eph]);
    const wsB = makeWorkspace("b", [other]);
    const keys = scrollbackLeafKeys([wsA, wsB]);
    const expected = [...leaves(normal.tree), ...leaves(other.tree)]
      .map((l) => l.key)
      .sort();
    expect([...keys].sort()).toEqual(expected);
    expect(keys.has(eph.focusedLeafKey)).toBe(false);
  });
  it("includes every leaf when no tab is ephemeral", () => {
    const a = makeTab(); // leaf-1
    const b = makeTab(); // leaf-2
    const keys = scrollbackLeafKeys([makeWorkspace("w", [a, b])]);
    expect([...keys].sort()).toEqual(["leaf-1", "leaf-2"]);
  });
});

describe("deleteWorkspaceFrom", () => {
  it("removes a workspace and points active at the left neighbour", () => {
    const w1 = makeWorkspace("a", [makeTab()]);
    const w2 = makeWorkspace("b", [makeTab()]);
    const w3 = makeWorkspace("c", [makeTab()]);
    const res = deleteWorkspaceFrom([w1, w2, w3], w2.id, () =>
      makeWorkspace("default", [makeTab()]),
    );
    expect(res.workspaces.map((w) => w.id)).toEqual([w1.id, w3.id]);
    expect(res.nextActiveId).toBe(w1.id);
  });
  it("replaces the last workspace with a fresh default", () => {
    const only = makeWorkspace("a", [makeTab()]);
    const fresh = makeWorkspace("default", [makeTab()]);
    const res = deleteWorkspaceFrom([only], only.id, () => fresh);
    expect(res.workspaces).toEqual([fresh]);
    expect(res.nextActiveId).toBe(fresh.id);
  });
});

describe("unreadCountForLeaves", () => {
  it("sums per-leaf unread counts, treating missing leaves as zero", () => {
    const unread = { "leaf-1": 2, "leaf-2": 1, "leaf-9": 5 };
    expect(unreadCountForLeaves(["leaf-1", "leaf-2"], unread)).toBe(3);
    expect(unreadCountForLeaves(["leaf-1", "leaf-absent"], unread)).toBe(2);
    expect(unreadCountForLeaves([], unread)).toBe(0);
  });
});

describe("reorderWorkspaces", () => {
  const wsList = (n: number) =>
    Array.from({ length: n }, (_, i) => makeWorkspace(`w${i}`, [makeTab()]));

  it("moves a workspace down with a correct index shift", () => {
    const out = reorderWorkspaces(wsList(4), 0, 2);
    expect(out.map((w) => w.name)).toEqual(["w1", "w2", "w0", "w3"]);
  });
  it("moves a workspace up", () => {
    const out = reorderWorkspaces(wsList(4), 3, 1);
    expect(out.map((w) => w.name)).toEqual(["w0", "w3", "w1", "w2"]);
  });
  it("moves to the head and to the tail", () => {
    const ws = wsList(4);
    expect(reorderWorkspaces(ws, 2, 0).map((w) => w.name)).toEqual([
      "w2", "w0", "w1", "w3",
    ]);
    expect(reorderWorkspaces(ws, 0, 3).map((w) => w.name)).toEqual([
      "w1", "w2", "w3", "w0",
    ]);
  });
  it("returns the same reference on a no-op (from === to)", () => {
    const ws = wsList(3);
    expect(reorderWorkspaces(ws, 1, 1)).toBe(ws);
  });
  it("returns the same reference on out-of-range indices", () => {
    const ws = wsList(3);
    expect(reorderWorkspaces(ws, -1, 1)).toBe(ws);
    expect(reorderWorkspaces(ws, 1, 5)).toBe(ws);
  });
  it("returns the same reference for a single-element list", () => {
    const ws = wsList(1);
    expect(reorderWorkspaces(ws, 0, 0)).toBe(ws);
  });
});

describe("insertionIndex", () => {
  const mids = [10, 30, 50, 70]; // four rows, midpoints in order

  it("returns 0 when the pointer is above the first row", () => {
    expect(insertionIndex(5, mids, 2)).toBe(0);
  });
  it("returns the last index when the pointer is below the last row", () => {
    expect(insertionIndex(100, mids, 1)).toBe(3);
  });
  it("is a no-op over the dragged row's own band", () => {
    expect(insertionIndex(30, mids, 1)).toBe(1);
    expect(insertionIndex(35, mids, 1)).toBe(1);
  });
  it("resolves to the gap between two other rows", () => {
    expect(insertionIndex(45, mids, 0)).toBe(1); // drag row 0 down between 1 and 2
    expect(insertionIndex(45, mids, 3)).toBe(2); // drag row 3 up between 1 and 2
  });
  it("handles an empty list", () => {
    expect(insertionIndex(10, [], 0)).toBe(0);
  });
});

describe("sourceLeafForNewTab", () => {
  it("returns the focused leaf of the target workspace's active tab", () => {
    const t1 = makeTab();
    const t2 = makeTab();
    const ws = { ...makeWorkspace("w", [t1, t2]), activeTabId: t2.id };
    expect(sourceLeafForNewTab([ws], ws.id)).toBe(t2.focusedLeafKey);
  });
  it("reads the target workspace, not the first/other one", () => {
    const a = makeTab();
    const b = makeTab();
    const wsA = makeWorkspace("a", [a]);
    const wsB = makeWorkspace("b", [b]);
    expect(sourceLeafForNewTab([wsA, wsB], wsB.id)).toBe(b.focusedLeafKey);
  });
  it("returns null for an unknown workspace", () => {
    const ws = makeWorkspace("w", [makeTab()]);
    expect(sourceLeafForNewTab([ws], "nope")).toBeNull();
  });
});

describe("flattenRaised", () => {
  it("lists raised panes in workspace→tab→leaf order", () => {
    const a = makeTab(); // leaf-1
    const b = makeTab(); // leaf-2
    const c = makeTab(); // leaf-3
    const wsA = makeWorkspace("a", [a, b]);
    const wsB = makeWorkspace("b", [c]);
    const att = { "leaf-1": "raised", "leaf-2": "idle", "leaf-3": "raised" };
    expect(flattenRaised([wsA, wsB], att)).toEqual([
      { wsId: wsA.id, tabId: a.id, key: "leaf-1" },
      { wsId: wsB.id, tabId: c.id, key: "leaf-3" },
    ]);
  });
});

describe("attentionPriority", () => {
  it("ranks question/permission co-equal, then alert, then finished, then error/unknown", () => {
    // Tiers extended by automations U12/R18: alert sits between the fast
    // unlocks (question/permission) and the parked long turnaround (finished).
    expect(attentionPriority("question")).toBe(0);
    expect(attentionPriority("permission")).toBe(0);
    expect(attentionPriority("alert")).toBe(1);
    expect(attentionPriority("finished")).toBe(2);
    expect(attentionPriority("error")).toBe(3);
    expect(attentionPriority(null)).toBe(3);
    expect(attentionPriority(undefined)).toBe(3);
  });
});

describe("attentionKind", () => {
  it("maps finished to done and everything else to input", () => {
    expect(attentionKind("finished")).toBe("done");
    expect(attentionKind("question")).toBe("input");
    expect(attentionKind("permission")).toBe("input");
    // Alert/error/unknown must read urgent, never as a calm completion.
    expect(attentionKind("alert")).toBe("input");
    expect(attentionKind("error")).toBe("input");
    expect(attentionKind(null)).toBe("input");
    expect(attentionKind(undefined)).toBe("input");
  });
});

describe("combineAttentionKinds", () => {
  it("lets input win over done, and done over nothing", () => {
    expect(combineAttentionKinds(["done", "input", "done"])).toBe("input");
    expect(combineAttentionKinds([null, "done", null])).toBe("done");
    expect(combineAttentionKinds([null, null])).toBe(null);
    expect(combineAttentionKinds([])).toBe(null);
  });
});

describe("rollupAttentionKind", () => {
  const att = {
    asking: "raised",
    stopped: "raised",
    seen: "acknowledged",
    quiet: "idle",
  };
  const reasons = {
    asking: "permission" as const,
    stopped: "finished" as const,
    seen: "question" as const,
  };

  it("reports input when any raised leaf is blocked on the user", () => {
    expect(rollupAttentionKind(["stopped", "asking"], att, reasons)).toBe("input");
  });

  it("reports done when the only raised leaves finished", () => {
    expect(rollupAttentionKind(["stopped", "quiet"], att, reasons)).toBe("done");
  });

  it("ignores non-raised leaves, matching the old boolean dot", () => {
    // An acknowledged question contributes nothing — you already saw it.
    expect(rollupAttentionKind(["seen", "quiet"], att, reasons)).toBe(null);
    expect(rollupAttentionKind([], att, reasons)).toBe(null);
  });

  it("treats a raised leaf with no recorded reason as input", () => {
    expect(rollupAttentionKind(["mystery"], { mystery: "raised" }, {})).toBe("input");
  });
});

describe("sortByAttentionPriority", () => {
  const list = (...keys: string[]) => keys.map((key) => ({ key }));

  it("orders question/permission before finished, then error/unknown", () => {
    const out = sortByAttentionPriority(list("a", "b", "c", "d"), {
      a: "finished",
      b: "question",
      c: "error",
      d: "permission",
    });
    expect(out.map((e) => e.key)).toEqual(["b", "d", "a", "c"]);
  });

  it("orders an automation alert between the fast unlocks and finished (U12/R18)", () => {
    const out = sortByAttentionPriority(list("a", "b", "c", "d"), {
      a: "finished",
      b: "alert",
      c: "question",
      d: "error",
    });
    expect(out.map((e) => e.key)).toEqual(["c", "b", "a", "d"]);
  });

  it("preserves positional order within a tier (AE2 tie-break)", () => {
    // Two co-equal top-tier reasons keep their input order.
    const out = sortByAttentionPriority(list("first", "second"), {
      first: "question",
      second: "permission",
    });
    expect(out.map((e) => e.key)).toEqual(["first", "second"]);
  });

  it("treats an absent reason as lowest priority", () => {
    const out = sortByAttentionPriority(list("known", "missing"), {
      known: "finished",
    });
    expect(out.map((e) => e.key)).toEqual(["known", "missing"]);
  });

  it("leaves an already-ordered list unchanged and does not mutate the input", () => {
    const input = list("q", "f");
    const reasons = { q: "question" as const, f: "finished" as const };
    const out = sortByAttentionPriority(input, reasons);
    expect(out.map((e) => e.key)).toEqual(["q", "f"]);
    expect(input.map((e) => e.key)).toEqual(["q", "f"]);
    expect(out).not.toBe(input);
  });

  it("handles empty and single-entry lists", () => {
    expect(sortByAttentionPriority(list(), {})).toEqual([]);
    expect(sortByAttentionPriority(list("only"), { only: "finished" })).toEqual([
      { key: "only" },
    ]);
  });
});
