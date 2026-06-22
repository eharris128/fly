// Session serialization (U12, U16). The backend stores this blob opaquely; the
// frontend owns the layout tree and the workspace grouping, so rehydration
// happens here.
import { invoke } from "@tauri-apps/api/core";
import type { Node } from "./layout";
import { coerceNotifications, type Notification } from "./notifications";

export interface SavedPane {
  cwd: string | null;
  title: string | null;
}
export interface SavedTab {
  tree: Node;
  panes: Record<string, SavedPane>;
  // Manual tab name; null = auto-named from the pane cwd (U16).
  title: string | null;
}
export interface SavedWorkspace {
  name: string;
  tabs: SavedTab[];
  activeTabIndex: number;
}
export interface SavedSession {
  workspaces: SavedWorkspace[];
  activeWorkspaceIndex: number;
  sidebarCollapsed: boolean;
  // Notification history (U20). Keyed by leafKey, so it resolves to a pane after
  // restart; bodies persist only when saveScrollback is on (gated in App).
  notifications: Notification[];
}

export function saveSession(session: SavedSession): Promise<void> {
  return invoke("save_session", { layout: session });
}

/**
 * Coerce a raw saved blob into the current `SavedSession` shape, or null if it's
 * unusable (caller falls back to a fresh default workspace, R14). Two shapes are
 * accepted so an upgrade never loses a user's tabs:
 *  - current: `{ workspaces, activeWorkspaceIndex, sidebarCollapsed }`
 *  - legacy (pre-workspaces): `{ tabs, activeIndex }` — wrapped in one "default"
 *    workspace. The backend wrapper version is unchanged (it only ever stored an
 *    opaque layout), so this migration is purely frontend.
 */
export function migrateSession(raw: unknown): SavedSession | null {
  if (!raw || typeof raw !== "object") return null;
  const v = raw as Record<string, unknown>;

  if (Array.isArray(v.workspaces)) {
    return {
      workspaces: v.workspaces as SavedWorkspace[],
      activeWorkspaceIndex:
        typeof v.activeWorkspaceIndex === "number" ? v.activeWorkspaceIndex : 0,
      sidebarCollapsed: v.sidebarCollapsed === true,
      // Tolerant: missing → empty history; malformed entries dropped.
      notifications: coerceNotifications(v.notifications),
    };
  }

  if (Array.isArray(v.tabs)) {
    const tabs = (v.tabs as Array<Record<string, unknown>>).map((t) => ({
      tree: t.tree as Node,
      panes: (t.panes ?? {}) as Record<string, SavedPane>,
      title: typeof t.title === "string" ? t.title : null,
    }));
    return {
      workspaces: [
        {
          name: "default",
          tabs,
          activeTabIndex:
            typeof v.activeIndex === "number" ? v.activeIndex : 0,
        },
      ],
      activeWorkspaceIndex: 0,
      sidebarCollapsed: false,
      // Legacy sessions predate notifications → empty history.
      notifications: coerceNotifications(v.notifications),
    };
  }

  return null;
}

export async function loadSession(): Promise<SavedSession | null> {
  const v = await invoke<unknown>("load_session");
  return migrateSession(v);
}
