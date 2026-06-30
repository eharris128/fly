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
// On top of that base, a pane that would otherwise read `idle` but has live
// background work (`liveTaskCount > 0`, already rise-debounced upstream by App)
// is upgraded to `running` — purely additive, so it only ever replaces `idle` and
// never competes with `working`/`waiting` (KTD1 of the running-state plan).
// Reserving `waiting` for unseen attention keeps a fresh, never-tasked claude
// (which pings "ready for input" while you're looking at it → acknowledged) from
// reading as `waiting`. A pane that exits drops out automatically — its
// foreground process is no longer `claude`, so `isAgent` goes false next poll.

import { leaves } from "./layout";
import { tabDisplayTitle, type Workspace } from "./workspaces";
import type { PaneActivity, UsageLimit } from "../ipc";

export type AgentStatus = "working" | "waiting" | "idle" | "running";

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
  /**
   * Effective (rise-debounced) count of live background task groups — the
   * `running · N tasks` number. `0` for any non-`running` row (a `> 0` count is
   * exactly what upgrades an `idle` base to `running`).
   */
  liveTaskCount: number;
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

/**
 * The dashboard's per-row status (see header). Computes today's base from
 * attention + output stretch, then applies the one additive upgrade: a base of
 * `idle` with live background work becomes `running`. `waiting` and `working` are
 * never reached by the upgrade, so the new state cannot regress them (R4, KTD1).
 * `liveTaskCount` is the already-effective (rise-debounced) count.
 */
function rowStatus(
  att: string | undefined,
  workingForMs: number | null,
  liveTaskCount: number,
): AgentStatus {
  const base: AgentStatus =
    att === "raised"
      ? "waiting" // finished + unseen → needs you
      : att === "acknowledged"
        ? "idle" // you're viewing it → parked
        : workingForMs != null
          ? "working"
          : "idle";
  return base === "idle" && liveTaskCount > 0 ? "running" : base;
}

/**
 * Build the grouped dashboard model: workspaces → tabs → agent rows. Pure over
 * the four App-held maps. Only `isAgent` leaves become rows; empty tabs and
 * workspaces are dropped (so `[]` ⟺ no agents running, the R7 empty state).
 *
 * The post-turn flicker grace (KTD-E) and the background-task rise-debounce
 * (KTD5) are both applied upstream by App — it zeroes a lingering `workingForMs`
 * and overwrites `liveTaskCount` with its effective value before this runs — so
 * the `status` rule here is just `rowStatus` over already-effective inputs
 * (raised → waiting, acknowledged → idle, else stretch → working, then the
 * additive idle → running upgrade).
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
        // Already rise-debounced by App (mirrors the workingForMs grace); a
        // `> 0` value here upgrades an idle base to `running`.
        const liveTaskCount = activity.liveTaskCount;
        rows.push({
          wsId: ws.id,
          tabId: tab.id,
          leafKey: leaf.key,
          tabTitle: title,
          cwd: cwdByLeaf[leaf.key] ?? null,
          workingForMs,
          liveTaskCount,
          // Only "raised" (unseen) is urgent needs-you; acknowledged is "seen".
          needsAttention: att === "raised",
          status: rowStatus(att, workingForMs, liveTaskCount),
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

/**
 * Rise-debounce a raw background-task count (running-state plan KTD5, R5). A
 * count only "counts" once it has persisted past `windowMs` since it first rose
 * above zero (`riseAt`), so a transient turn-start helper spawn, a pid-reuse blip,
 * or a resume/restart process swap does not flash `running`. The fall is
 * immediate: App clears `riseAt` the instant the raw count returns to 0, so a `0`
 * raw here yields `0` regardless of `riseAt` (R7 — no fall debounce).
 *
 * Pure: `now` and `riseAt` are injected. App owns the per-leaf `riseAt` map and
 * overwrites each PaneActivity's `liveTaskCount` with this result before
 * `buildHomeModel`, so the model reads an already-effective count (same upstream-
 * massage shape as `effectiveAttention` / the `workingForMs` grace).
 */
export function effectiveTaskCount(
  raw: number,
  riseAt: number | null,
  now: number,
  windowMs: number,
): number {
  return raw > 0 && riseAt != null && now - riseAt >= windowMs ? raw : 0;
}

/** Total agent rows across the model (for an at-a-glance count / empty check). */
export function agentCount(model: HomeWorkspaceGroup[]): number {
  let n = 0;
  for (const ws of model) for (const tab of ws.tabs) n += tab.rows.length;
  return n;
}

/**
 * Resolve a dashboard workspace-number keypress (U7) to a jump target: the first
 * tab's first agent row in the Nth *displayed* workspace group. `n` is 1-based,
 * to match the on-screen badges (workspace 1..N) and the `leader 1-9` tab chord —
 * and it indexes the model, so agent-less workspaces (dropped by `buildHomeModel`)
 * are skipped, not counted. Returns null when `n` is out of range, so the caller
 * no-ops on an unmapped digit. Every group has ≥1 tab and every tab ≥1 row
 * (empties are dropped), so a resolved target always lands on a real agent pane,
 * never a bare tab.
 */
export function workspaceJumpTarget(
  model: HomeWorkspaceGroup[],
  n: number,
): { wsId: string; tabId: string; leafKey: string } | null {
  const ws = model[n - 1];
  if (!ws) return null;
  const tab = ws.tabs[0];
  return { wsId: ws.wsId, tabId: tab.tabId, leafKey: tab.rows[0].leafKey };
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

/**
 * Pluralize the background-task count for the `running` row's label
 * (running-state plan R2): `1 → "1 task"`, otherwise `"N tasks"`. A pure
 * presentational helper the HomeView imports — mirrors `formatDuration`'s shape,
 * so the dashboard's `running · N tasks` reading stays unit-tested.
 */
export function formatTaskCount(n: number): string {
  return `${n} ${n === 1 ? "task" : "tasks"}`;
}

/**
 * Human label for a `/usage` plan-limit gauge (usage panel). Maps the known
 * `kind`s Claude Code returns to the same wording `/usage` shows; a per-model
 * (`weekly_scoped`) limit folds in its model name, and an unknown kind degrades
 * to a humanized form of the kind string so a new limit type still renders.
 */
export function usageLimitLabel(limit: Pick<UsageLimit, "kind" | "scopeLabel">): string {
  switch (limit.kind) {
    case "session":
      return "Session";
    case "weekly_all":
      return "Weekly · all models";
    case "weekly_scoped":
      return limit.scopeLabel ? `Weekly · ${limit.scopeLabel}` : "Weekly · scoped";
    case "overage":
      return "Usage credits";
    default:
      if (limit.scopeLabel) return `Weekly · ${limit.scopeLabel}`;
      if (!limit.kind) return "Usage";
      // Humanize: "some_new_kind" → "Some new kind".
      const words = limit.kind.replace(/_/g, " ");
      return words.charAt(0).toUpperCase() + words.slice(1);
  }
}

/**
 * Absolute reset time for a limit's ISO 8601 reset timestamp, formatted to match
 * Claude Code's `/usage`: `"Resets 7:50am (America/Cancun)"` for a reset later
 * today, `"Resets Jul 3, 8am (America/Cancun)"` for one on another day. The hour
 * drops `:00` ("8am", not "8:00am"); the `Mon D` date is shown only when the
 * reset falls on a different local calendar day than `nowMs`; the IANA zone is
 * appended. `timeZone` is injectable for deterministic tests and defaults to the
 * browser's resolved zone. Returns null for a null/unparseable timestamp so the
 * caller omits the line. Pure (`nowMs` + `timeZone` injected).
 */
export function formatResetTime(
  resetsAt: string | null,
  nowMs: number,
  timeZone?: string,
): string | null {
  if (!resetsAt) return null;
  const resetMs = Date.parse(resetsAt);
  if (Number.isNaN(resetMs)) return null;
  const tz = timeZone ?? Intl.DateTimeFormat().resolvedOptions().timeZone;
  const reset = new Date(resetMs);

  // Time: "7:50am" / "8am" (drop :00), lowercase meridiem, no space — as /usage.
  const timeParts = new Intl.DateTimeFormat("en-US", {
    timeZone: tz,
    hour: "numeric",
    minute: "2-digit",
    hour12: true,
  }).formatToParts(reset);
  const part = (t: string) => timeParts.find((p) => p.type === t)?.value ?? "";
  const hour = part("hour");
  const minute = part("minute");
  const meridiem = part("dayPeriod").toLowerCase();
  const time = minute === "00" ? `${hour}${meridiem}` : `${hour}:${minute}${meridiem}`;

  // Date prefix only when the reset is on a different local calendar day.
  const dayKey = new Intl.DateTimeFormat("en-US", {
    timeZone: tz,
    year: "numeric",
    month: "numeric",
    day: "numeric",
  });
  const datePrefix =
    dayKey.format(reset) === dayKey.format(new Date(nowMs))
      ? ""
      : `${new Intl.DateTimeFormat("en-US", { timeZone: tz, month: "short", day: "numeric" }).format(reset)}, `;

  return `Resets ${datePrefix}${time} (${tz})`;
}
