import { describe, it, expect } from "vitest";
import { migrateSession, toSavedWorkspaces } from "./serialize";
import type { Node } from "./layout";
import type { Tab, Workspace } from "./workspaces";

const leaf: Node = { kind: "leaf", key: "leaf-1" };

// Live-model makers for toSavedWorkspaces (the projection input is the
// in-memory workspace shape, not the saved one).
let seq = 0;
function liveTab(key: string, extra: Partial<Tab> = {}): Tab {
  return {
    id: `tab-${++seq}`,
    tree: { kind: "leaf", key },
    focusedLeafKey: key,
    title: null,
    ...extra,
  };
}
function liveWs(name: string, tabs: Tab[], activeTabId?: string): Workspace {
  return {
    id: `ws-${++seq}`,
    name,
    tabs,
    activeTabId: activeTabId ?? tabs[0]?.id ?? "",
  };
}

describe("migrateSession", () => {
  it("passes a current-shape session through", () => {
    const session = {
      workspaces: [
        { name: "web", tabs: [{ tree: leaf, panes: {}, title: "api" }], activeTabIndex: 0 },
      ],
      activeWorkspaceIndex: 0,
      sidebarCollapsed: true,
      notifications: [],
    };
    expect(migrateSession(session)).toEqual(session);
  });

  it("defaults missing current-shape fields", () => {
    const out = migrateSession({ workspaces: [] });
    expect(out).toEqual({
      workspaces: [],
      activeWorkspaceIndex: 0,
      sidebarCollapsed: false,
      notifications: [],
    });
  });

  it("carries valid notifications through and drops malformed ones", () => {
    const out = migrateSession({
      workspaces: [],
      notifications: [
        { id: 1, leafKey: "leaf-1", reason: "permission", title: "t", body: null, ts: 9, state: "unread" },
        { id: "bad", leafKey: "leaf-2", reason: "permission", ts: 9 },
      ],
    });
    expect(out?.notifications).toHaveLength(1);
    expect(out?.notifications[0].id).toBe(1);
  });

  it("wraps a legacy {tabs, activeIndex} blob in one default workspace", () => {
    const legacy = {
      tabs: [
        { tree: leaf, panes: { "leaf-1": { cwd: "/home/alice", title: null } } },
        { tree: leaf, panes: {} },
      ],
      activeIndex: 1,
    };
    const out = migrateSession(legacy);
    expect(out).toEqual({
      workspaces: [
        {
          name: "default",
          tabs: [
            { tree: leaf, panes: { "leaf-1": { cwd: "/home/alice", title: null } }, title: null },
            { tree: leaf, panes: {}, title: null },
          ],
          activeTabIndex: 1,
        },
      ],
      activeWorkspaceIndex: 0,
      sidebarCollapsed: false,
      notifications: [],
    });
  });

  it("preserves a legacy tab's existing title and tolerates a missing activeIndex", () => {
    const out = migrateSession({ tabs: [{ tree: leaf, panes: {}, title: "kept" }] });
    expect(out?.workspaces[0].tabs[0].title).toBe("kept");
    expect(out?.workspaces[0].activeTabIndex).toBe(0);
  });

  it("returns null for junk", () => {
    expect(migrateSession(null)).toBeNull();
    expect(migrateSession(undefined)).toBeNull();
    expect(migrateSession("nope")).toBeNull();
    expect(migrateSession({})).toBeNull();
    expect(migrateSession({ other: 1 })).toBeNull();
  });

  it("deserializes a pre-U11 session (no ephemeral flag anywhere) unaffected (KTD-G)", () => {
    // The flag never serializes, so old documents are indistinguishable from
    // new ones — no version bump, nothing to migrate.
    const preU11 = {
      workspaces: [
        { name: "web", tabs: [{ tree: leaf, panes: {}, title: null }], activeTabIndex: 0 },
      ],
      activeWorkspaceIndex: 0,
      sidebarCollapsed: false,
      notifications: [],
    };
    const out = migrateSession(preU11);
    expect(out).toEqual(preU11);
    expect(JSON.stringify(out)).not.toContain("ephemeral");
  });

  it("accepts a workspace with zero tabs — an only-ephemeral workspace at save time (U11)", () => {
    // The restore path refills such a workspace with a fresh default tab
    // (App.svelte's "never an empty workspace" guard), so the document is valid.
    const out = migrateSession({
      workspaces: [{ name: "only", tabs: [], activeTabIndex: 0 }],
      activeWorkspaceIndex: 0,
    });
    expect(out?.workspaces).toEqual([{ name: "only", tabs: [], activeTabIndex: 0 }]);
  });
});

describe("toSavedWorkspaces", () => {
  it("omits an ephemeral tab from the saved session while sibling tabs persist (R12)", () => {
    const a = liveTab("leaf-1");
    const eph = liveTab("leaf-2", { ephemeral: true });
    const b = liveTab("leaf-3", { title: "named" });
    const out = toSavedWorkspaces([liveWs("w", [a, eph, b])], {
      "leaf-1": { cwd: "/home/alice", title: null },
      "leaf-2": { cwd: "/tmp", title: null },
      "leaf-3": { cwd: null, title: null },
    });
    expect(out).toEqual([
      {
        name: "w",
        tabs: [
          {
            tree: a.tree,
            panes: { "leaf-1": { cwd: "/home/alice", title: null } },
            title: null,
          },
          { tree: b.tree, panes: { "leaf-3": { cwd: null, title: null } }, title: "named" },
        ],
        activeTabIndex: 0,
      },
    ]);
  });

  it("never serializes the ephemeral flag or the ephemeral tab's leaves (KTD-G)", () => {
    const out = toSavedWorkspaces(
      [liveWs("w", [liveTab("leaf-1"), liveTab("leaf-2", { ephemeral: true })])],
      {},
    );
    const json = JSON.stringify(out);
    expect(json).not.toContain("ephemeral");
    expect(json).not.toContain("leaf-2");
  });

  it("defaults a leaf missing from the snapshot to a null pane", () => {
    const out = toSavedWorkspaces([liveWs("w", [liveTab("leaf-1")])], {});
    expect(out[0].tabs[0].panes).toEqual({ "leaf-1": { cwd: null, title: null } });
  });

  it("keeps a workspace whose only tab is ephemeral as a valid empty-tabs document (U11 edge)", () => {
    const out = toSavedWorkspaces([liveWs("only", [liveTab("leaf-1", { ephemeral: true })])], {});
    // Deliberate: the workspace survives by name with tabs: [] rather than
    // being dropped — restore's "never an empty workspace" guard refills it
    // with a fresh tab, the same rule closeTabIn applies to a last-tab close.
    expect(out).toEqual([{ name: "only", tabs: [], activeTabIndex: 0 }]);
    // Round-trip safety: the loader accepts the document unchanged.
    const migrated = migrateSession({ workspaces: out, activeWorkspaceIndex: 0 });
    expect(migrated?.workspaces).toEqual(out);
  });

  it("falls back to the left persisted neighbour when the active tab is ephemeral (U11 edge)", () => {
    const a = liveTab("leaf-1");
    const eph = liveTab("leaf-2", { ephemeral: true });
    const b = liveTab("leaf-3");
    const ws = liveWs("w", [a, eph, b], eph.id);
    expect(toSavedWorkspaces([ws], {})[0].activeTabIndex).toBe(0); // a, the left neighbour
  });

  it("clamps to the first persisted tab when an ephemeral active tab has no left neighbour", () => {
    const eph = liveTab("leaf-1", { ephemeral: true });
    const p = liveTab("leaf-2");
    const ws = liveWs("w", [eph, p], eph.id);
    expect(toSavedWorkspaces([ws], {})[0].activeTabIndex).toBe(0); // p, at index 0 once eph is dropped
  });

  it("remaps a persisted active tab's index across dropped ephemeral siblings", () => {
    const eph = liveTab("leaf-1", { ephemeral: true });
    const p1 = liveTab("leaf-2");
    const p2 = liveTab("leaf-3");
    const ws = liveWs("w", [eph, p1, p2], p2.id);
    expect(toSavedWorkspaces([ws], {})[0].activeTabIndex).toBe(1); // p2 within [p1, p2]
  });
});

describe("Automations workspace role marker (U6, R2)", () => {
  it("toSavedWorkspaces persists the role marker", () => {
    const marked: Workspace = {
      ...liveWs("Automations", [liveTab("leaf-1")]),
      role: "automations",
    };
    expect(toSavedWorkspaces([marked], {})[0].role).toBe("automations");
  });

  it("a normal workspace serializes without a role (dropped by JSON)", () => {
    const out = toSavedWorkspaces([liveWs("web", [liveTab("leaf-1")])], {});
    expect(out[0].role).toBeUndefined();
    expect(JSON.stringify(out)).not.toContain("automations");
  });

  it("migrateSession preserves role on a current-shape session", () => {
    const saved = {
      workspaces: [
        {
          name: "Automations",
          tabs: [{ tree: leaf, panes: {}, title: null }],
          activeTabIndex: 0,
          role: "automations",
        },
      ],
      activeWorkspaceIndex: 0,
      sidebarCollapsed: false,
      notifications: [],
    };
    expect(migrateSession(saved)?.workspaces[0].role).toBe("automations");
  });

  it("a legacy {tabs, activeIndex} session yields an unmarked default workspace", () => {
    const out = migrateSession({
      tabs: [{ tree: leaf, panes: {}, title: null }],
      activeIndex: 0,
    });
    expect(out?.workspaces[0].role).toBeUndefined();
  });

  it("a current-shape session without role restores with role undefined (back-compat)", () => {
    const out = migrateSession({
      workspaces: [{ name: "web", tabs: [], activeTabIndex: 0 }],
      activeWorkspaceIndex: 0,
    });
    expect(out?.workspaces[0].role).toBeUndefined();
  });
});
