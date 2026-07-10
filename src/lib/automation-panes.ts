// Placement helpers for automation agent-run tabs (U8, R9). Pure and
// unit-tested, mirroring layout.ts / workspaces.ts: App.svelte owns the live
// `$state`, the leaf/tab id minting, and the maps read at mount; this module
// only decides *where* an incoming agent run's tab should land.

import { leaves } from "./layout";
import type { Workspace } from "./workspaces";
import type { RunStatus } from "../ipc";

/**
 * The id of the durable **Automations** workspace — the first workspace marked
 * `role === "automations"` (automations-workspace-and-model U6, R2) — or `null`
 * when none is marked yet. Resolution keys on the persisted `role`, never the
 * in-memory `id` (which resets every launch), so an automation run always lands
 * in the same workspace across restarts. Defensive "first" pick: a session
 * should hold at most one marked workspace, but if two ever exist the earliest
 * wins (`App.ensureAutomationsWorkspace` also dedupes on create, U7).
 */
export function findAutomationsWorkspace(
  workspaces: Workspace[],
): string | null {
  return workspaces.find((w) => w.role === "automations")?.id ?? null;
}

/** The resolved launch flags an agent run carries (from the backend, U4a). */
export interface AgentLaunch {
  model: string | null;
  effort: string | null;
  fallbackModel: string | null;
}

/**
 * The argv a background automation agent pane launches with (automations-
 * workspace-and-model U7, R11): `claude --dangerously-skip-permissions` plus
 * `--model` / `--effort` / `--fallback-model` for each launch flag the backend
 * resolved (omitted when null), with the prompt **last** — a positional after
 * every flag, mirroring the handoff argv-ordering lesson so no variadic flag
 * swallows it. All flags null ⇒ exactly today's `["claude",
 * "--dangerously-skip-permissions", prompt]`.
 */
export function buildAgentArgv(prompt: string, launch: AgentLaunch): string[] {
  const argv = ["claude", "--dangerously-skip-permissions"];
  if (launch.model) argv.push("--model", launch.model);
  if (launch.effort) argv.push("--effort", launch.effort);
  if (launch.fallbackModel) argv.push("--fallback-model", launch.fallbackModel);
  argv.push(prompt);
  return argv;
}

/**
 * Whether a closed automation run's background tab should auto-close (U8,
 * R6/R7): only a `succeeded` run with no *genuine* outstanding raise. The
 * backend suppresses an automation pane's normal completion raise (KTD5), so a
 * leftover `isRaised` here means the agent asked for something mid-run — keep
 * the tab for review (R7). A `failed` run always keeps its tab (R7). Pure.
 */
export function shouldAutoCloseRun(
  status: RunStatus,
  isRaised: boolean,
): boolean {
  return status === "succeeded" && !isRaised;
}

/** What the monitor-registered close should remove (monitor-handoff U6, R13):
 *  the whole tab when the registering pane is its sole leaf, else ONLY that
 *  pane's leaf — sibling split panes are unrelated live sessions and survive. */
export type MonitorCloseTarget =
  | { kind: "tab"; tabId: string }
  | { kind: "leaf"; tabId: string; leafKey: string };

/**
 * Resolve a registering pane id to its monitor-registered close target
 * (monitor-handoff U6, R13): paneId → leaf key (via the caller's reverse
 * index) → the enclosing tab, scanning every workspace — the monitor parent is
 * an ORDINARY pane, so it lives in normal workspaces/tabs, not the Automations
 * workspace. A sole-leaf tab closes whole (`kind: "tab"`); in a split tab the
 * residue is only the registering pane's leaf (`kind: "leaf"`) — closing the
 * whole tab would silently kill sibling panes' processes, the exact case the
 * user-initiated path gates behind the destructive confirm (requestCloseTab).
 * `null` when the pane id is unknown or its leaf no longer resolves (the tab
 * was already closed manually) — the caller no-ops. Note the reverse index may
 * hold a stale entry for an exited pane; the workspace scan is what makes the
 * lookup safe (a gone leaf → `null`, never a wrong target).
 */
export function monitorCloseTarget(
  workspaces: Workspace[],
  leafByPaneId: Record<number, string>,
  paneId: number,
): MonitorCloseTarget | null {
  const leafKey = leafByPaneId[paneId];
  if (!leafKey) return null;
  for (const ws of workspaces)
    for (const tab of ws.tabs)
      if (leaves(tab.tree).some((l) => l.key === leafKey))
        return leaves(tab.tree).length === 1
          ? { kind: "tab", tabId: tab.id }
          : { kind: "leaf", tabId: tab.id, leafKey };
  return null;
}
