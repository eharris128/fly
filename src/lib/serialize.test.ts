import { describe, it, expect } from "vitest";
import { migrateSession } from "./serialize";
import type { Node } from "./layout";

const leaf: Node = { kind: "leaf", key: "leaf-1" };

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
        { tree: leaf, panes: { "leaf-1": { cwd: "/home/evan", title: null } } },
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
            { tree: leaf, panes: { "leaf-1": { cwd: "/home/evan", title: null } }, title: null },
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
});
