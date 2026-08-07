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
  /**
   * The pane currently backing this leaf, or null while one is still being
   * assigned (phone-screenshot-drop U4, R14/KTD6).
   *
   * Unlike `leafKey` this is *identity*, not addressing. Leaf keys are stable
   * across respawn by design — the backend resolves a key to the newest live
   * pane — so a leaf whose agent exited and was replaced silently resolves to
   * the replacement. Pane ids are monotonic and never reused, so a consumer
   * that echoes this back on a mutation can be told it targeted a session that
   * no longer exists.
   *
   * Unlike `lastReplyAt`/`questionPendingAt`, this one IS pushed: the leaf→pane
   * mapping lives in the webview, which is the only place that knows it.
   */
  paneId: number | null;
  /**
   * Whether the human opted this pane into receiving peer messages
   * (agent-peer-messaging U3, R6/KTD6). Pushed — the dashboard toggle is
   * deliberately the ONLY writer: no socket op, CLI verb, or feed route can
   * set it, so a prompt-injected agent cannot opt itself (or its victim) in.
   * Session-scoped: App seeds the map empty every launch (nothing persisted
   * for a same-uid process to edit), so every launch starts closed.
   */
  peerOptIn: boolean;
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
  /**
   * The free-text answer primitive (feed-other-answer R2): the digit that
   * focuses the picker's own "Type something." row. Present only when a
   * `mode:"other"` answer — `POST {text, mode:"other", ifAskedAt}` — can be
   * delivered against this question; fly owns the digit → text → Enter
   * keystroke choreography. Absent when unavailable (unanswerable shape,
   * digit unknown) — consumers must not offer free-text answer UX then.
   */
  otherKey?: string;
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
   * Provenance (feed-question-screen-fallback R5; hook-ask-channel KTD3):
   * `"hook"` when the body came from a live `PermissionRequest` hook holding
   * fly's socket — the primary source, exposed with no attention-reason
   * corroboration (the held connection is the proof), `askedAt` = fly's
   * receipt stamp. A hook-sourced *permission* body is the one shape a
   * `mode:"decision"` answer — `POST {mode:"decision", decision:"allow"|
   * "deny", ifAskedAt}`, opt-in-gated like every remote permission answer —
   * can resolve (through the hook's own response channel, no keystrokes; a
   * choice picker still answers via keys/other). `"screen"` when the body was
   * synthesized from the pane's rendered terminal grid (Claude Code ≥ 2.1.206
   * no longer flushes the ask to the transcript at ask time); absent for a
   * transcript-derived body. A screen body's `askedAt` is the ask-time raise
   * stamp; a permission-kind screen body also carries the rendered options in
   * `questions` (a transcript-derived permission never does).
   */
  source?: "screen" | "hook";
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

/**
 * The last run's healthcheck verdict on the wire (mirrors Rust `VerdictEntry`,
 * U6). A verdict is parsed only from a run whose infra outcome succeeded, so a
 * `fail` verdict rides a `lastStatus: "succeeded"` row — read the verdict, not
 * the status, for honest pass/fail.
 */
export interface VerdictEntry {
  /** `"pass"` | `"fail"` — the lowercase wire spelling. */
  outcome: string;
  /** The check's short verdict note; empty string when the parse had none. */
  note: string;
}

/** One automation on the wire (mirrors Rust `AutomationEntry`). Backend-filled. */
export interface AutomationEntry {
  id: string;
  name: string;
  cron: string;
  timezone: string;
  enabled: boolean;
  /** Whether this automation is a monitor (bounded healthcheck) — U6. */
  monitor: boolean;
  /** Effective closed-loop disposition (headless-agent-automations R11):
   * true when its runs are backend-owned `claude -p` children (no pane).
   * Additive — an older backend omits it, so read it absent-tolerant. */
  headless?: boolean;
  nextRunAt: number | null;
  lastStatus: string | null;
  lastRunAt: number | null;
  /** When a parsed verdict retired this monitor (epoch ms); null otherwise. U6. */
  retiredAt: number | null;
  /** The last run's parsed verdict; absent when the run carried none. U6. */
  lastVerdict?: VerdictEntry;
  /** The dependency edge's upstream automation id (automation-dependencies
   * R16); absent for ordinary automations. Note `lastStatus` may also read
   * `"withheld"` — the dependent honestly declined to run. Additive. */
  after?: string;
  /** The fly-minted decline reason when the last run is withheld; absent
   * otherwise (automation-dependencies R16). */
  lastWithheldReason?: string;
}

/** The full SSE frame (mirrors Rust `FeedSnapshot`). */
export interface FeedSnapshot {
  version: number;
  emittedAt: number;
  /**
   * Epoch ms of the webview's last roster push — *not* when this frame was
   * emitted (phone-screenshot-drop U4, KTD6); null before the first push.
   *
   * `emittedAt` only proves the backend is alive: frames keep flowing on the
   * SSE keepalive even if the webview has frozen, and the backend never clears
   * its roster cache on webview teardown. A consumer that acts on roster
   * contents must therefore check *this* stamp for staleness, or it will keep
   * targeting panes that stopped existing some time ago.
   *
   * It advances on every push, including one whose roster is unchanged (which
   * deliberately does not bump `version`) — that is what separates an idle-live
   * webview from a dead one.
   */
  publishedAt: number | null;
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
 *
 * `paneByLeaf` is the app's live leaf→pane map, passed in rather than reached
 * for so this module stays framework-free and testable (phone-screenshot-drop
 * U4). A leaf missing from it yields `paneId: null` — the entry is still
 * published, because dropping it would make a starting agent vanish from the
 * roster rather than merely be untargetable.
 *
 * `peerOptInByLeaf` is the session-scoped peer-receive consent map
 * (agent-peer-messaging U3): App owns it, the dashboard toggle is its only
 * writer, and a leaf missing from it is closed — the KTD6 default.
 */
export function buildFeedPayload(
  model: HomeWorkspaceGroup[],
  paneByLeaf: Readonly<Record<string, number>> = {},
  peerOptInByLeaf: Readonly<Record<string, boolean>> = {},
): FeedPublishPayload {
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
          paneId: paneByLeaf[row.leafKey] ?? null,
          peerOptIn: peerOptInByLeaf[row.leafKey] ?? false,
        });
      }
    }
  }
  return { agents };
}
