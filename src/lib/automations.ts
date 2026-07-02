// Automations dashboard view-model (U10, R25). A pure data module like
// `home.ts` / `workspaces.ts` — no DOM, no Svelte — so it is unit-tested
// without an app. App fetches the `AutomationsDashboard` (list + store health)
// from the backend on dashboard open and refetches on `automation://changed`;
// this module turns the raw `Automation[]` into the static, sorted, humanized
// rows the HomeView renders below the agent list.
//
// The panel is read-only: unlike agent rows there is no keyboard selection or
// (in v1) jump — the pane_id→leaf jump mapping lives in App.svelte and is left
// for a later pass. `linkedPaneId` is derived here so that affordance can be
// wired without touching the view-model.

import type { Automation, AutomationMode, RunStatus } from "../ipc";

/** Last-run status for the row: the last run's status, or `"never run"`. */
export type LastStatus = "never run" | RunStatus;

export interface AutomationRow {
  id: string;
  name: string;
  /** Agent vs script, for a small mode tag. */
  mode: AutomationMode;
  /** Coarse humanized schedule + timezone, e.g. `"every 5 min · America/New_York"`. */
  schedule: string;
  /** True when `nextRunAt` is null — the automation is paused (R23). */
  paused: boolean;
  /** Relative next-occurrence time (`"in 5 minutes"`), or null when paused. */
  nextRun: string | null;
  /** Derived from the last run row (R25) — `"never run"` when there is none. */
  lastStatus: LastStatus;
  /** Relative last-run time (`"5 minutes ago"`), or null when never run. */
  lastRun: string | null;
  /** Last run's error/skip reason, or null. */
  lastError: string | null;
  /** The last run's linked pane (agent runs), for a future jump affordance. */
  linkedPaneId: number | null;
}

/**
 * Build the sorted, humanized dashboard rows from the raw automation list.
 * Sort mirrors the CLI's `load_store_at` (U9): next-run ascending, paused
 * (`nextRunAt == null`) last, ties broken by name — so the dashboard and
 * `fly automation list` agree. Pure over `nowMs` (injected for tests).
 */
export function automationsToRows(
  automations: Automation[],
  nowMs: number,
): AutomationRow[] {
  const sorted = [...automations].sort((a, b) => {
    const an = a.nextRunAt;
    const bn = b.nextRunAt;
    if (an != null && bn != null) return an - bn || a.name.localeCompare(b.name);
    if (an != null) return -1; // a is scheduled, b paused → a first
    if (bn != null) return 1; // b is scheduled, a paused → b first
    return a.name.localeCompare(b.name); // both paused → by name
  });
  return sorted.map((a) => toRow(a, nowMs));
}

function toRow(a: Automation, nowMs: number): AutomationRow {
  // Last-run state is *derived* from the history's last row (R25) — no separate
  // mirror to drift, matching the Rust model's `last_run()`.
  const last = a.runs.length > 0 ? a.runs[a.runs.length - 1] : null;
  const lastRunAt = last ? (last.finishedAt ?? last.startedAt) : null;
  return {
    id: a.id,
    name: a.name,
    mode: a.mode.kind,
    schedule: humanSchedule(a.cron, a.timezone),
    paused: a.nextRunAt == null,
    nextRun: a.nextRunAt != null ? relativeTime(a.nextRunAt, nowMs) : null,
    lastStatus: last ? last.status : "never run",
    lastRun: lastRunAt != null ? relativeTime(lastRunAt, nowMs) : null,
    lastError: last?.error ?? null,
    linkedPaneId: last?.paneId ?? null,
  };
}

/**
 * Coarse-humanize a 5-field cron + timezone for a glance: recognizes the common
 * shapes (`every minute`, `every N min`, `hourly`, `daily`, `weekly`,
 * `monthly`) and otherwise falls back to the raw cron expression. The IANA
 * timezone is always appended (` · <tz>`) since a schedule is meaningless
 * without it. Anything not a clean 5-field expression passes through verbatim.
 */
export function humanSchedule(cron: string, tz: string): string {
  return `${coarseCron(cron)} · ${tz}`;
}

function coarseCron(cron: string): string {
  const raw = cron.trim();
  const parts = raw.split(/\s+/);
  if (parts.length !== 5) return raw;
  const [min, hour, dom, mon, dow] = parts;
  const wild = (f: string) => f === "*";
  const fixed = (f: string) => /^\d+$/.test(f);

  // */N in the minute field, everything else wild → "every N min" ("every
  // minute" for */1 or a bare *).
  const everyN = /^\*\/(\d+)$/.exec(min);
  if (wild(hour) && wild(dom) && wild(mon) && wild(dow)) {
    if (wild(min) || min === "*/1") return "every minute";
    if (everyN) return `every ${everyN[1]} min`;
    if (fixed(min)) return "hourly"; // fixed minute, every hour
  }
  // Fixed minute+hour, day fields decide the cadence.
  if (fixed(min) && fixed(hour)) {
    if (wild(dom) && wild(mon) && wild(dow)) return "daily";
    if (wild(dom) && wild(mon) && fixed(dow)) return "weekly";
    if (fixed(dom) && wild(mon) && wild(dow)) return "monthly";
  }
  return raw;
}

/** Full/singular unit word, e.g. `plural(1, "minute") === "minute"`. */
function plural(n: number, unit: string): string {
  return n === 1 ? unit : `${unit}s`;
}

/**
 * Relative time for a dashboard row: `"just now"` within 45s, else
 * `"in 5 minutes"` (future) / `"5 minutes ago"` (past), coarsening to hours
 * then days. Richer than the CLI's `rel_label` (full words + pluralization),
 * per model.rs's note that "the dashboard does the richer humanization".
 *
 * The past/future branch keeps the magnitude non-negative and picks the suffix
 * — mirroring `rel_label`'s pattern (JS numbers don't wrap, but the branch is
 * still what makes direction correct, and it guards the release-overflow-checks
 * concern for parity with the Rust side).
 */
export function relativeTime(targetMs: number, nowMs: number): string {
  const future = targetMs >= nowMs;
  const deltaMs = future ? targetMs - nowMs : nowMs - targetMs;
  const secs = Math.floor(deltaMs / 1000);
  if (secs < 45) return "just now";

  const mins = Math.round(secs / 60);
  let body: string;
  if (mins < 60) {
    body = `${mins} ${plural(mins, "minute")}`;
  } else {
    const hours = Math.round(mins / 60);
    if (hours < 24) {
      body = `${hours} ${plural(hours, "hour")}`;
    } else {
      const days = Math.round(hours / 24);
      body = `${days} ${plural(days, "day")}`;
    }
  }
  return future ? `in ${body}` : `${body} ago`;
}
