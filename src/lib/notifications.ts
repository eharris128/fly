// Notification history model (U20, KTD16). A pure data module like
// `workspaces.ts` / `layout.ts` — no DOM, no Svelte — so it is unit-tested
// without an app. The backend is the policy authority and emits a seed event
// when policy says `record`; the frontend owns this list and the
// received → unread → read → cleared lifecycle (a UI concept).
//
// Entries are keyed by **leafKey**, not the per-spawn-ephemeral paneId, so a
// restored session's history still resolves to its pane after paneIds are
// reassigned (App resolves paneId → leafKey at ingestion). "Cleared" is the
// terminal lifecycle state: clearing *removes* the entry (it must reach disk),
// so the stored `state` is only "unread" | "read".

import type { AttentionReason } from "../ipc";

export type NotificationState = "unread" | "read";

export interface Notification {
  /** Backend monotonic id — the dedupe key. */
  id: number;
  /** Stable layout key the entry resolves to (survives paneId reassignment). */
  leafKey: string;
  reason: AttentionReason;
  title: string | null;
  /** Agent output; persisted only when `saveScrollback` is on (privacy). */
  body: string | null;
  /** Wall-clock ms (from the backend), for ordering + relative time. */
  ts: number;
  state: NotificationState;
}

/** The ingested seed event, with `paneId` already resolved to a `leafKey`. */
export interface IngestedNotification {
  id: number;
  leafKey: string;
  reason: AttentionReason;
  title: string | null;
  body: string | null;
  ts: number;
  /** Backend-authored read-at-birth (the user was viewing the pane). */
  read: boolean;
}

/**
 * Append an ingested notification, honoring its backend-authored `read` bit for
 * the birth state (KTD16 — no frontend re-derivation). Deduped by `id`, so a
 * doubly-delivered event is idempotent.
 */
export function addNotification(
  list: Notification[],
  ev: IngestedNotification,
): Notification[] {
  if (list.some((n) => n.id === ev.id)) return list;
  return [
    ...list,
    {
      id: ev.id,
      leafKey: ev.leafKey,
      reason: ev.reason,
      title: ev.title,
      body: ev.body,
      ts: ev.ts,
      state: ev.read ? "read" : "unread",
    },
  ];
}

/** Mark specific entries (by id) read — e.g. a click in the panel. Returns the
 *  original array unchanged when nothing flips, to avoid spurious reactivity. */
export function markRead(list: Notification[], ids: number[]): Notification[] {
  const set = new Set(ids);
  let changed = false;
  const next = list.map((n) => {
    if (set.has(n.id) && n.state === "unread") {
      changed = true;
      return { ...n, state: "read" as const };
    }
    return n;
  });
  return changed ? next : list;
}

/**
 * Mark every unread entry for the given leaf keys read — the "user viewed the
 * tab" transition (App passes the tab's leaves). Only the *later* read
 * transition; auto-read-at-birth is backend-authored on `addNotification`.
 */
export function markReadForLeaves(
  list: Notification[],
  leafKeys: Iterable<string>,
): Notification[] {
  const set = new Set(leafKeys);
  let changed = false;
  const next = list.map((n) => {
    if (set.has(n.leafKey) && n.state === "unread") {
      changed = true;
      return { ...n, state: "read" as const };
    }
    return n;
  });
  return changed ? next : list;
}

/** Mark all unread entries read ("mark all read" panel action). */
export function markAllRead(list: Notification[]): Notification[] {
  let changed = false;
  const next = list.map((n) => {
    if (n.state === "unread") {
      changed = true;
      return { ...n, state: "read" as const };
    }
    return n;
  });
  return changed ? next : list;
}

/** Clear specific entries (by id) — removed from the list and from disk. */
export function clear(list: Notification[], ids: number[]): Notification[] {
  const set = new Set(ids);
  const next = list.filter((n) => !set.has(n.id));
  return next.length === list.length ? list : next;
}

/**
 * Clear (remove) every entry for the given leaf keys — the "user viewed the tab"
 * removal (U5). Unlike `markReadForLeaves` (which flips unread→read but keeps the
 * entry as history), this *removes* the entries, so they leave the panel and the
 * unread badges and the removal reaches disk via `toPersisted`. Returns the
 * original array unchanged when nothing matched, to avoid spurious reactivity.
 */
export function clearForLeaves(
  list: Notification[],
  leafKeys: Iterable<string>,
): Notification[] {
  const set = new Set(leafKeys);
  const next = list.filter((n) => !set.has(n.leafKey));
  return next.length === list.length ? list : next;
}

/** Clear the whole history. */
export function clearAll(): Notification[] {
  return [];
}

/**
 * Drop notifications whose leaf no longer exists (a tab/workspace was deleted),
 * so orphaned unread counts don't leak and `newestUnread` can't dead-end on a
 * leaf that resolves to nothing.
 */
export function pruneToLeaves(
  list: Notification[],
  liveLeafKeys: Set<string>,
): Notification[] {
  return list.filter((n) => liveLeafKeys.has(n.leafKey));
}

/** Unread count per leaf key — the rollup badges/counts derive from. */
export function unreadByLeaf(list: Notification[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const n of list)
    if (n.state === "unread") out[n.leafKey] = (out[n.leafKey] ?? 0) + 1;
  return out;
}

/** Total unread (for the control-bar badge). */
export function unreadTotal(list: Notification[]): number {
  let n = 0;
  for (const e of list) if (e.state === "unread") n++;
  return n;
}

/**
 * The most-recently-added unread entry (by `ts`, ties broken by `id`), for
 * "jump to newest unread" (R22). `null` when nothing is unread.
 */
export function newestUnread(list: Notification[]): Notification | null {
  let best: Notification | null = null;
  for (const n of list) {
    if (n.state !== "unread") continue;
    if (best === null || n.ts > best.ts || (n.ts === best.ts && n.id > best.id)) {
      best = n;
    }
  }
  return best;
}

/**
 * The persisted shape: bodies are dropped unless `includeBodies` (they can carry
 * agent output, so default is metadata-only — KTD16 privacy). Titles/reason/ts
 * always persist so the panel stays useful across a restart.
 */
export function toPersisted(
  list: Notification[],
  includeBodies: boolean,
): Notification[] {
  return list.map((n) => ({ ...n, body: includeBodies ? n.body : null }));
}

const REASONS: AttentionReason[] = ["question", "permission", "finished", "error"];

function coerceOne(raw: unknown): Notification | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  if (typeof o.id !== "number") return null;
  if (typeof o.leafKey !== "string") return null;
  if (!REASONS.includes(o.reason as AttentionReason)) return null;
  if (typeof o.ts !== "number") return null;
  return {
    id: o.id,
    leafKey: o.leafKey,
    reason: o.reason as AttentionReason,
    title: typeof o.title === "string" ? o.title : null,
    body: typeof o.body === "string" ? o.body : null,
    ts: o.ts,
    // An unknown/missing state restores as unread (a missed notification stays
    // visible rather than being silently swallowed).
    state: o.state === "read" ? "read" : "unread",
  };
}

/** A notification augmented for display: the source label + whether its pane
 *  still resolves to a live tab (for the jump). Built by App from the live tree. */
export interface NotificationView extends Notification {
  source: string;
  jumpable: boolean;
}

/** Compact relative time ("now", "5s ago", "3m ago", "2h ago", "4d ago"). Pure:
 *  `now` is injected so it is testable and never trips the workflow clock ban. */
export function relativeTime(ts: number, now: number): string {
  const s = Math.max(0, Math.floor((now - ts) / 1000));
  if (s < 5) return "now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/** Coerce a raw persisted array into valid notifications, dropping malformed. */
export function coerceNotifications(raw: unknown): Notification[] {
  if (!Array.isArray(raw)) return [];
  const out: Notification[] = [];
  for (const r of raw) {
    const n = coerceOne(r);
    if (n) out.push(n);
  }
  return out;
}
