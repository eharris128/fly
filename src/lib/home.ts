// Agent dashboard view-model (U5). A pure data module like `workspaces.ts` /
// `layout.ts` — no DOM, no Svelte — so it is unit-tested without an app. App
// holds the live `agentByLeaf`/`cwdByLeaf`/`attentionByLeaf` maps and the
// workspace tree; this module turns them into the grouped, labelled rows the
// HomeView renders.
//
// Only panes detected as running Claude Code (`isAgent`) become rows; tabs and
// workspaces with no agent row are omitted, so an empty model (no agents) is the
// R7 empty state. The per-row `status` follows the KTD-E precedence:
// attention wins (`waiting`), then an active output stretch (`working`), else
// `idle`. A pane that exits drops out automatically — its foreground process is
// no longer `claude`, so `isAgent` goes false on the next poll.

import { leaves } from "./layout";
import { tabDisplayTitle, type Workspace } from "./workspaces";
import type { PaneActivity } from "../ipc";

export type AgentStatus = "working" | "waiting" | "idle";

export interface AgentRow {
  wsId: string;
  tabId: string;
  leafKey: string;
  /** The owning tab's display title (auto-named from cwd or a manual override). */
  tabTitle: string;
  /** This pane's own cwd, for a per-row label when a tab holds split panes. */
  cwd: string | null;
  /** Current work stretch in ms, or null when idle. */
  workingForMs: number | null;
  /** The pane is raised/acknowledged in the attention model (needs the user). */
  needsAttention: boolean;
  status: AgentStatus;
}

export interface HomeTabGroup {
  tabId: string;
  title: string;
  rows: AgentRow[];
}

export interface HomeWorkspaceGroup {
  wsId: string;
  name: string;
  tabs: HomeTabGroup[];
}

/** Raised/acknowledged both mean "the user is needed" for the dashboard label. */
function isAttentive(state: string | undefined): boolean {
  return state === "raised" || state === "acknowledged";
}

/**
 * Build the grouped dashboard model: workspaces → tabs → agent rows. Pure over
 * the four App-held maps. Only `isAgent` leaves become rows; empty tabs and
 * workspaces are dropped (so `[]` ⟺ no agents running, the R7 empty state).
 *
 * The post-turn flicker grace (KTD-E) is applied upstream by App — it zeroes a
 * lingering `workingForMs` before this runs — so the `status` rule here is a
 * straight precedence: attention → `waiting`, else a stretch → `working`, else
 * `idle`.
 */
export function buildHomeModel(
  workspaces: Workspace[],
  agentByLeaf: Record<string, PaneActivity>,
  cwdByLeaf: Record<string, string | null>,
  attentionByLeaf: Record<string, string>,
): HomeWorkspaceGroup[] {
  const out: HomeWorkspaceGroup[] = [];
  for (const ws of workspaces) {
    const tabs: HomeTabGroup[] = [];
    for (const tab of ws.tabs) {
      const title = tabDisplayTitle(tab, cwdByLeaf);
      const rows: AgentRow[] = [];
      for (const leaf of leaves(tab.tree)) {
        const activity = agentByLeaf[leaf.key];
        if (!activity?.isAgent) continue;
        const needsAttention = isAttentive(attentionByLeaf[leaf.key]);
        const workingForMs = activity.workingForMs;
        const status: AgentStatus = needsAttention
          ? "waiting"
          : workingForMs != null
            ? "working"
            : "idle";
        rows.push({
          wsId: ws.id,
          tabId: tab.id,
          leafKey: leaf.key,
          tabTitle: title,
          cwd: cwdByLeaf[leaf.key] ?? null,
          workingForMs,
          needsAttention,
          status,
        });
      }
      if (rows.length > 0) tabs.push({ tabId: tab.id, title, rows });
    }
    if (tabs.length > 0) out.push({ wsId: ws.id, name: ws.name, tabs });
  }
  return out;
}

/** Total agent rows across the model (for an at-a-glance count / empty check). */
export function agentCount(model: HomeWorkspaceGroup[]): number {
  let n = 0;
  for (const ws of model) for (const tab of ws.tabs) n += tab.rows.length;
  return n;
}

/**
 * Compact elapsed duration: `"0s"`, `"45s"`, `"5m"`, `"1h 5m"`, `"2h"`. Minutes
 * and hours floor (a glance metric, not a stopwatch). Negative inputs clamp to
 * `"0s"`. Mirrors the relative-time style in `notifications.ts`.
 */
export function formatDuration(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  if (totalSec < 60) return `${totalSec}s`;
  const totalMin = Math.floor(totalSec / 60);
  if (totalMin < 60) return `${totalMin}m`;
  const h = Math.floor(totalMin / 60);
  const m = totalMin % 60;
  return m > 0 ? `${h}h ${m}m` : `${h}h`;
}
