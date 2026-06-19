import { describe, it, expect, beforeEach } from "vitest";
import { newLeaf, splitLeaf, resetKeys, type Node } from "./layout";
import {
  basename,
  tabDisplayTitle,
  findTab,
  closeTabIn,
  deleteWorkspaceFrom,
  flattenRaised,
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
