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
  /**
   * Epoch ms of the agent's pending question, or null when nothing is pending
   * (feed-pending-question R4). Backend-stamped at emit like `lastReplyAt`:
   * for a choice question it equals `/output`'s `question.askedAt`; for a
   * permission question it is best-effort. A changed value means a new
   * question. The pushed roster always carries null.
   */
  questionPendingAt: number | null;
}

/**
 * One selectable option of a pending question (mirrors Rust `QuestionOption`).
 * Rides `GET /agents/{key}/output` — the frontend never populates these; they
 * document the boundary for the external consumer.
 */
export interface QuestionOption {
  /**
   * The answer primitive: the 1-based digit the on-screen picker binds this
   * option to. To answer an `answerable` question, POST this string as
   * `mode:"keys"` input — no need to reverse-engineer the keybinding.
   */
  key: string;
  label: string;
  description: string;
}

/** One question of a pending AskUserQuestion batch (mirrors Rust `QuestionSpec`). */
export interface QuestionSpec {
  question: string;
  header: string;
  multiSelect: boolean;
  options: QuestionOption[];
}

/**
 * The pending question on `GET /agents/{key}/output` (mirrors Rust
 * `QuestionBody`; feed-pending-question R1/R7). Absent when nothing is
 * pending. `answerable` is true only for the one v1-answerable shape — a
 * single single-select question; consumers must not build answer UX otherwise.
 */
export interface QuestionBody {
  askedAt: number;
  kind: "choice" | "permission";
  tool: string;
  answerable: boolean;
  context?: string;
  questions?: QuestionSpec[];
  request?: string;
  /**
   * Provenance (feed-question-screen-fallback R5): `"screen"` when the body
   * was synthesized from the pane's rendered terminal grid (Claude Code ≥
   * 2.1.206 no longer flushes the ask to the transcript at ask time); absent
   * for a transcript-derived body. A screen body's `askedAt` is the ask-time
   * raise stamp; a permission-kind screen body also carries the rendered
   * options in `questions` (a transcript-derived permission never does).
   */
  source?: "screen";
}

/**
 * One turn of the recent conversation tail on `GET /agents/{key}/output`
 * (mirrors Rust `TurnEntry`; feed-conversation-tail). `role` is exactly
 * `"user"` (a prompt delivered TO the agent, from any source) or `"agent"`
 * (a reply FROM it); `at` is epoch ms, always present; `text` is scrubbed,
 * sanitized, and char-capped (truncation carries no marker).
 */
export interface TurnEntry {
  role: "user" | "agent";
  at: number;
  text: string;
}

/**
 * `GET /agents/{key}/output` response body (mirrors Rust `AgentOutputBody`).
 * Empty `text` with no `repliedAt` is the "no reply yet" state; `question` is
 * present only while the agent is blocked on one (feed-pending-question);
 * `turns` is the recent conversation tail, oldest → newest, ending with the
 * current reply (its `at` equals `repliedAt`) — absent, never empty, when
 * there is no servable history (feed-conversation-tail). The frontend never
 * populates this — it documents the boundary for the consumer.
 */
export interface AgentOutputBody {
  text: string;
  repliedAt?: number;
  question?: QuestionBody;
  turns?: TurnEntry[];
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
          questionPendingAt: null, // backend-stamped at emit; never pushed
        });
      }
    }
  }
  return { agents };
}
