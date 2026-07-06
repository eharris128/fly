// Feed wire contract (U1). A pure data module like `home.ts` — no DOM, no
// Svelte — so it is unit-tested without an app. It mirrors the Rust wire shape
// in `src-tauri/src/feed/wire.rs`: the frontend PUSHES the agent roster (which
// only the webview computes, folding topology + status + debounce), and the
// backend fills the automations half + `version`/`emittedAt` before it streams
// the snapshot to the local `game` consumer over SSE.
//
// `buildFeedPayload` flattens the dashboard's grouped model (`HomeWorkspaceGroup[]`
// from `home.ts`) into the flat `AgentEntry[]` the wire carries — reusing the
// exact status/attention values the dashboard shows, so the feed can never drift
// from what fly itself displays.

import type { HomeWorkspaceGroup } from "./home";
import type { AttentionReason } from "../ipc";

/** One in-flight agent on the wire (mirrors Rust `AgentEntry`). */
export interface AgentEntry {
  leafKey: string;
  workspace: string;
  tab: string;
  cwd: string | null;
  status: string;
  needsAttention: boolean;
  reason: AttentionReason | null;
  workingForMs: number | null;
  liveTaskCount: number;
  num: number | null;
  /**
   * Epoch ms of the agent's most recent reply, or null if it has never
   * replied (feed-agent-reply-io U1). **Backend-stamped at frame emit** from
   * the same resolver behind `GET /agents/{key}/output`, so it always matches
   * that endpoint's `repliedAt`; the pushed roster always carries null.
   */
  lastReplyAt: number | null;
}

/** One automation on the wire (mirrors Rust `AutomationEntry`). Backend-filled. */
export interface AutomationEntry {
  id: string;
  name: string;
  cron: string;
  timezone: string;
  enabled: boolean;
  nextRunAt: number | null;
  lastStatus: string | null;
  lastRunAt: number | null;
}

/** The full SSE frame (mirrors Rust `FeedSnapshot`). */
export interface FeedSnapshot {
  version: number;
  emittedAt: number;
  agents: AgentEntry[];
  automations: AutomationEntry[];
}

/** What the frontend pushes to the backend each poll — just the agent half. */
export interface FeedPublishPayload {
  agents: AgentEntry[];
}

/**
 * Flatten the dashboard's grouped model into the flat `AgentEntry[]` the wire
 * carries. Pure: the same rows the HomeView renders, one entry per agent pane,
 * carrying its owning workspace name + tab title for the consumer's grouping.
 */
export function buildFeedPayload(model: HomeWorkspaceGroup[]): FeedPublishPayload {
  const agents: AgentEntry[] = [];
  for (const ws of model) {
    for (const tab of ws.tabs) {
      for (const row of tab.rows) {
        agents.push({
          leafKey: row.leafKey,
          workspace: ws.name,
          tab: tab.title,
          cwd: row.cwd,
          status: row.status,
          needsAttention: row.needsAttention,
          reason: row.reason,
          workingForMs: row.workingForMs,
          liveTaskCount: row.liveTaskCount,
          num: row.num ?? null,
          lastReplyAt: null, // backend-stamped at emit; never pushed
        });
      }
    }
  }
  return { agents };
}
