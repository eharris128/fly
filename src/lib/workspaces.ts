// Workspace + tab model (U16). A *workspace* is a named collection of tabs; a
// *tab* owns a split tree of panes. This mirrors `layout.ts`: the operations
// here are pure (they take id factories rather than touching a global counter)
// and return new arrays, so they're unit-tested without a running app. App.svelte
// holds the live `$state` and the id counters and stays a thin orchestrator.

import { leaves, type Node } from "./layout";

export interface Tab {
  id: string; // in-memory only (e.g. "tab-3"); never persisted
  tree: Node;
  focusedLeafKey: string;
  // null = auto-name from the pane cwd; a string is a manual override that
  // sticks and stops tracking the cwd (see `tabDisplayTitle`).
  title: string | null;
}

export interface Workspace {
  id: string; // in-memory only (e.g. "ws-2"); never persisted
  name: string;
  tabs: Tab[];
  // Remembered per-workspace so switching back restores the last active tab.
  activeTabId: string;
}

/** Last path segment of a cwd, for an auto tab name. "/" → "/". */
export function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  if (trimmed === "") return "/";
  return trimmed.split("/").pop() ?? "";
}

/**
 * A tab's visible name: a manual `title` if set, else the basename of the first
 * known cwd among the focused leaf then the rest (left-to-right), else "shell".
 * The single source of truth for how a tab is labelled (sidebar + breadcrumb).
 */
export function tabDisplayTitle(
  tab: Tab,
  cwdByLeaf: Record<string, string | null>,
): string {
  if (tab.title && tab.title.trim()) return tab.title.trim();
  const keys = [tab.focusedLeafKey, ...leaves(tab.tree).map((l) => l.key)];
  for (const k of keys) {
    const cwd = cwdByLeaf[k];
    if (cwd) {
      const base = basename(cwd);
      if (base) return base;
    }
  }
  return "shell";
}

/** Locate a tab and its owning workspace by tab id. */
export function findTab(
  workspaces: Workspace[],
  tabId: string,
): { ws: Workspace; tab: Tab } | null {
  for (const ws of workspaces) {
    const tab = ws.tabs.find((t) => t.id === tabId);
    if (tab) return { ws, tab };
  }
  return null;
}

/**
 * Remove tab `tabId` from whichever workspace owns it. If it was that
 * workspace's last tab, a fresh one (from `makeTab`) takes its place — a
 * workspace is never left empty, mirroring the tab-level "never zero tabs"
 * rule. The owning workspace's `activeTabId` is moved to the neighbour when the
 * closed tab was active. `activeWorkspaceId` is unaffected, so the caller's
 * active-tab derivation just follows the updated `activeTabId`.
 */
export function closeTabIn(
  workspaces: Workspace[],
  tabId: string,
  makeTab: () => Tab,
): Workspace[] {
  return workspaces.map((ws) => {
    const idx = ws.tabs.findIndex((t) => t.id === tabId);
    if (idx === -1) return ws;
    const remaining = ws.tabs.filter((t) => t.id !== tabId);
    if (remaining.length === 0) {
      const fresh = makeTab();
      return { ...ws, tabs: [fresh], activeTabId: fresh.id };
    }
    const activeTabId =
      ws.activeTabId === tabId
        ? remaining[Math.min(Math.max(0, idx - 1), remaining.length - 1)].id
        : ws.activeTabId;
    return { ...ws, tabs: remaining, activeTabId };
  });
}

/**
 * Remove workspace `wsId`. If it was the only workspace, a fresh default
 * (from `makeDefault`) replaces it — there is always at least one workspace.
 * Returns the new list plus the id to activate when the deleted workspace was
 * the active one (the previous neighbour, or the fresh default).
 */
export function deleteWorkspaceFrom(
  workspaces: Workspace[],
  wsId: string,
  makeDefault: () => Workspace,
): { workspaces: Workspace[]; nextActiveId: string } {
  const idx = workspaces.findIndex((w) => w.id === wsId);
  if (idx === -1) return { workspaces, nextActiveId: workspaces[0]?.id ?? "" };
  const remaining = workspaces.filter((w) => w.id !== wsId);
  if (remaining.length === 0) {
    const fresh = makeDefault();
    return { workspaces: [fresh], nextActiveId: fresh.id };
  }
  const nextActiveId = remaining[Math.max(0, idx - 1)].id;
  return { workspaces: remaining, nextActiveId };
}

/**
 * Sum unread notification counts over a set of leaf keys (a tab's or a
 * workspace's leaves), from the per-leaf rollup. The tab/workspace badge counts
 * derive from this — the notification analogue of the `attention` flag rollup.
 */
export function unreadCountForLeaves(
  leafKeys: Iterable<string>,
  unreadByLeaf: Record<string, number>,
): number {
  let n = 0;
  for (const k of leafKeys) n += unreadByLeaf[k] ?? 0;
  return n;
}

/**
 * Every raised pane across all workspaces, in stable workspace→tab→leaf order.
 * The ordering source for cross-workspace attention cycling (leader u).
 */
export function flattenRaised(
  workspaces: Workspace[],
  attentionByLeaf: Record<string, string>,
): { wsId: string; tabId: string; key: string }[] {
  const out: { wsId: string; tabId: string; key: string }[] = [];
  for (const ws of workspaces)
    for (const tab of ws.tabs)
      for (const l of leaves(tab.tree))
        if (attentionByLeaf[l.key] === "raised")
          out.push({ wsId: ws.id, tabId: tab.id, key: l.key });
  return out;
}
