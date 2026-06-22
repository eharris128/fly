// Agent dashboard view-model (U5). A pure data module like `workspaces.ts` /
// `layout.ts` — no DOM, no Svelte — so it is unit-tested without an app. App
// holds the live `agentByLeaf`/`cwdByLeaf`/`attentionByLeaf` maps and the
// workspace tree; this module turns them into the grouped, labelled rows the
// HomeView renders.
//
// Only panes detected as running Claude Code (`isAgent`) become rows; tabs and
// workspaces with no agent row are omitted, so an empty model (no agents) is the
// R7 empty state. The per-row `status` precedence (refined from KTD-E):
//   - `raised`       → `waiting`  (the agent needs you and you HAVEN'T looked)
//   - `acknowledged` → `idle`     (you're already viewing it — parked, not urgent,
//                                  and never a stray "working" from residual output)
//   - else a current output stretch → `working`, else `idle`.
// Reserving `waiting` for unseen attention keeps a fresh, never-tasked claude
// (which pings "ready for input" while you're looking at it → acknowledged) from
// reading as `waiting`. A pane that exits drops out automatically — its
// foreground process is no longer `claude`, so `isAgent` goes false next poll.

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

/** The dashboard's per-row status from attention + output stretch (see header). */
function rowStatus(att: string | undefined, workingForMs: number | null): AgentStatus {
  if (att === "raised") return "waiting"; // finished + unseen → needs you
  if (att === "acknowledged") return "idle"; // you're viewing it → parked
  return workingForMs != null ? "working" : "idle";
}

/**
 * Build the grouped dashboard model: workspaces → tabs → agent rows. Pure over
 * the four App-held maps. Only `isAgent` leaves become rows; empty tabs and
 * workspaces are dropped (so `[]` ⟺ no agents running, the R7 empty state).
 *
 * The post-turn flicker grace (KTD-E) is applied upstream by App — it zeroes a
 * lingering `workingForMs` before this runs — so the `status` rule here is just
 * `rowStatus` (raised → waiting, acknowledged → idle, else stretch → working).
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
        const att = attentionByLeaf[leaf.key];
        const workingForMs = activity.workingForMs;
        rows.push({
          wsId: ws.id,
          tabId: tab.id,
          leafKey: leaf.key,
          tabTitle: title,
          cwd: cwdByLeaf[leaf.key] ?? null,
          workingForMs,
          // Only "raised" (unseen) is urgent needs-you; acknowledged is "seen".
          needsAttention: att === "raised",
          status: rowStatus(att, workingForMs),
        });
      }
      if (rows.length > 0) tabs.push({ tabId: tab.id, title, rows });
    }
    if (tabs.length > 0) out.push({ wsId: ws.id, name: ws.name, tabs });
  }
  return out;
}

/**
 * Downgrade a *stale* `raised` attention to `idle` for the dashboard.
 *
 * Claude Code re-pings its "waiting for input" notification periodically while
 * idle. On an agent you've already dealt with, that repeat ping re-raises the
 * pane in the backend (acknowledged → raised), which would otherwise cycle the
 * dashboard row back to `waiting` even though the agent did no new work. A raise
 * only counts when the agent produced output *after* you last engaged with it
 * (viewed it → `acknowledged`, or typed → `idle`, recorded in `lastEngagedByLeaf`);
 * a raise with no newer output is a stale ping and presents as `idle`. Genuine
 * raises — a real turn that finished while you weren't looking — pass through.
 *
 * Pure: `now` and the engaged map are injected. App applies it before
 * `buildHomeModel` so the model's `raised → waiting` rule stays simple.
 */
export function effectiveAttention(
  attentionByLeaf: Record<string, string>,
  agentByLeaf: Record<string, PaneActivity>,
  lastEngagedByLeaf: Record<string, number>,
  now: number,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, att] of Object.entries(attentionByLeaf)) {
    if (att === "raised") {
      const ago = agentByLeaf[key]?.lastOutputAgoMs;
      const lastOutputAt = ago != null ? now - ago : Number.NEGATIVE_INFINITY;
      const engagedAt = lastEngagedByLeaf[key] ?? 0;
      if (lastOutputAt <= engagedAt) {
        out[key] = "idle"; // stale repeat ping on a parked agent — not "waiting"
        continue;
      }
    }
    out[key] = att;
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
