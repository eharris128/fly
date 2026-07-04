// Session serialization (U12, U16). The backend stores this blob opaquely; the
// frontend owns the layout tree and the workspace grouping, so rehydration
// happens here.
import { invoke } from "@tauri-apps/api/core";
import { leaves, type Node } from "./layout";
import { coerceNotifications, type Notification } from "./notifications";
import { persistedTabs, type Workspace } from "./workspaces";

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
  // Durable role marker (automations-workspace-and-model U6, R2/KTD1); absent on
  // a normal or pre-feature workspace. The one field that lets the Automations
  // workspace be found by role after a restart re-mints every in-memory id.
  role?: "automations";
}
export interface SavedSession {
  workspaces: SavedWorkspace[];
  activeWorkspaceIndex: number;
  sidebarCollapsed: boolean;
  // Notification history (U20). Keyed by leafKey, so it resolves to a pane after
  // restart; bodies persist only when saveScrollback is on (gated in App).
  notifications: Notification[];
}

/**
 * Saved `activeTabIndex` for a workspace: the active tab's index within its
 * *persisted* tabs. When the active tab is ephemeral (never saved, U-ID U11)
 * the index falls back to the nearest persisted tab before it — the same
 * left-neighbour rule `closeTabIn` applies when an active tab goes away —
 * clamped into range. All-ephemeral workspaces yield 0 (the restore path
 * refills them with a fresh tab; see `toSavedWorkspaces`).
 */
function persistedActiveTabIndex(ws: Workspace): number {
  const kept = persistedTabs(ws.tabs);
  const idx = kept.findIndex((t) => t.id === ws.activeTabId);
  if (idx !== -1) return idx;
  const liveIdx = ws.tabs.findIndex((t) => t.id === ws.activeTabId);
  let keptBefore = 0;
  for (let i = 0; i < liveIdx; i++) if (ws.tabs[i].ephemeral !== true) keptBefore++;
  return Math.max(0, Math.min(keptBefore - 1, kept.length - 1));
}

/**
 * Project live workspaces into the saved-session shape (U-ID U11, R12, KTD-G).
 * Ephemeral tabs are skipped entirely: `SavedPane` has no command field, so
 * they would restore as dead bare shells. The flag itself never serializes —
 * `SavedTab` has no field for it — so the schema is unchanged (no version
 * bump) and pre-U11 sessions round-trip untouched.
 *
 * `panesByLeaf` carries the per-leaf snapshot; the caller resolves live cwds
 * (async) up front so this projection stays pure and unit-testable. A leaf
 * missing from the snapshot degrades to `{ cwd: null, title: null }`.
 *
 * A workspace whose only tabs are ephemeral serializes with `tabs: []` — a
 * deliberately valid document rather than dropping the workspace: the restore
 * path already enforces "never an empty workspace" by inserting a fresh
 * default tab (the same rule `closeTabIn` applies to a last-tab close), so
 * the workspace survives by name with a fresh shell. Workspaces are never
 * dropped, so the saved `activeWorkspaceIndex` stays valid as-is.
 */
export function toSavedWorkspaces(
  workspaces: Workspace[],
  panesByLeaf: Record<string, SavedPane>,
): SavedWorkspace[] {
  return workspaces.map((ws) => ({
    name: ws.name,
    tabs: persistedTabs(ws.tabs).map((t) => ({
      tree: t.tree,
      panes: Object.fromEntries(
        leaves(t.tree).map((l) => [
          l.key,
          panesByLeaf[l.key] ?? { cwd: null, title: null },
        ]),
      ),
      title: t.title,
    })),
    activeTabIndex: persistedActiveTabIndex(ws),
    // Carry the durable role marker (U6). Undefined on normal workspaces →
    // dropped by JSON serialization, so the document is unchanged for them.
    role: ws.role,
  }));
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
