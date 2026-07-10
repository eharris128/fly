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

import type {
  Automation,
  AutomationMode,
  MonitorPointers,
  RunRow,
  RunStatus,
} from "../ipc";
import {
  buildMonitorPickupCommand,
  sanitizeTranscriptPath,
  stripControlChars,
} from "./handoff";

/** Last-run status for the row: the last run's status, or `"never run"`. */
export type LastStatus = "never run" | RunStatus;

/**
 * The monitor state badge (monitor-handoff U7, R18) — the CLI's
 * `monitor_state_label` spellings verbatim, so `fly automation list` and the
 * dashboard can never disagree. Precedence retired (split pass/fail by the
 * durable verdict row; plain `"retired"` is the defensive
 * retirement-without-a-verdict-row fallback) > broken (derived infra-failure
 * count at/past the R7 threshold) > paused (`nextRunAt` null) > parked.
 */
export type MonitorState =
  | "parked"
  | "paused"
  | "broken"
  | "retired pass"
  | "retired fail"
  | "retired";

/**
 * The backend-derived broken-monitor inputs (monitor-handoff U7): the
 * per-monitor consecutive-infra-failure counts and the one Rust threshold
 * constant, both riding the `AutomationsDashboard` DTO so nothing here
 * re-derives run-history walks or hardcodes the number.
 */
export interface MonitorDerivation {
  /** Monitor id → consecutive infra-failure count (`infraFailures` on the DTO). */
  infraFailures: Record<string, number>;
  /** `monitorBrokenThreshold` on the DTO (verdict.rs::MONITOR_BROKEN_THRESHOLD). */
  brokenThreshold: number;
}

export interface AutomationRow {
  id: string;
  name: string;
  /** Agent vs script, for a small mode tag. */
  mode: AutomationMode;
  /** Coarse humanized schedule + timezone, e.g. `"every 5 min · America/New_York"`. */
  schedule: string;
  /** True when `nextRunAt` is null — the automation is paused (R23). */
  paused: boolean;
  /** Opt-in interrupt resilience (interrupt-resilience U5/R1): a small "retry"
   * tag so the operator can see which automations re-run after a crash. */
  retryOnInterrupt: boolean;
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
  /** Configured launch model for an agent automation (automations-workspace-and-
   * model U9, R13); null = Claude's own default. Always null for scripts — the
   * `mode` field distinguishes "Claude default" (agent) from "—" (script). */
  model: string | null;
  /** Configured reasoning effort for an agent automation; null = Claude default. */
  effort: string | null;
  /** The model the *last* run actually launched with (R13), or null. */
  lastRunModel: string | null;
  /** The effort the last run actually launched with, or null. */
  lastRunEffort: string | null;
  /** Monitor flavor bit (monitor-handoff U7); false for ordinary automations. */
  monitor: boolean;
  /** Derived monitor state (R18); null for non-monitors (they render mode tags). */
  monitorState: MonitorState | null;
  /** The durable verdict's outcome/note (monitor-handoff R4), from the newest
   * verdict-bearing run row; note is control-char-sanitized for display.
   * Null until a verdict lands (and always for non-monitors). */
  verdictOutcome: "pass" | "fail" | null;
  verdictNote: string | null;
  /** The verdict run's failure-bundle path (R15), or null (incl. PASS and a
   * failed bundle write). */
  bundlePath: string | null;
  /** Registration-time pickup pointers (R11); what the R16 pickup spawns from. */
  pickupPointers: MonitorPointers | null;
}

/**
 * Build the sorted, humanized dashboard rows from the raw automation list.
 * Sort mirrors the CLI's `load_store_at` (U9, extended by monitor-handoff
 * U5/U7 — R18): bucket first (scheduled 0 — parked monitors ride with
 * recurring automations by next-run — then paused 1, then retired 2, the
 * CLI's `sort_bucket`), next-run ascending inside a bucket, ties broken by
 * name — so the dashboard and `fly automation list` agree. Pure over `nowMs`
 * (injected for tests). `monitor` carries the DTO's backend-derived
 * broken-monitor inputs; omitting it (legacy callers/tests) just means no
 * broken state can derive — every other monitor state still does.
 */
export function automationsToRows(
  automations: Automation[],
  nowMs: number,
  monitor?: MonitorDerivation,
): AutomationRow[] {
  const sorted = [...automations].sort(
    (a, b) =>
      sortBucket(a) - sortBucket(b) ||
      (a.nextRunAt ?? 0) - (b.nextRunAt ?? 0) ||
      a.name.localeCompare(b.name),
  );
  return sorted.map((a) => toRow(a, nowMs, monitor));
}

/**
 * The CLI's `sort_bucket` mirrored exactly (monitor-handoff U5/U7, R18):
 * 0 = scheduled (incl. parked monitors), 1 = paused, 2 = retired last.
 * Retirement is checked first — retire also clears `nextRunAt`, and a
 * retired row must never read as merely paused. Within a bucket `nextRunAt`
 * is either uniformly set (0) or uniformly null (1 and 2), so the next-run
 * comparison above only ever bites inside the scheduled bucket.
 */
function sortBucket(a: Automation): number {
  if (a.retiredAt != null) return 2;
  if (a.nextRunAt == null) return 1;
  return 0;
}

/**
 * The newest verdict-bearing run row (monitor-handoff R4) — the CLI's
 * `verdict_run` mirrored. A monitor retires on its first verdict so at most
 * one exists; newest-first keeps the read honest if that ever loosens.
 */
function verdictRun(a: Automation): RunRow | null {
  for (let i = a.runs.length - 1; i >= 0; i--) {
    if (a.runs[i].verdict != null) return a.runs[i];
  }
  return null;
}

/**
 * Derive a monitor's dashboard state (monitor-handoff U7, R18) — the CLI's
 * `monitor_state_label` mirrored exactly: null for non-monitors; otherwise
 * retired (pass/fail split by the durable verdict row, plain `"retired"` as
 * the defensive no-verdict-row fallback) > broken (derived count at/past the
 * threshold) > paused > parked. Exported for tests.
 */
export function monitorStateOf(
  a: Automation,
  monitor?: MonitorDerivation,
): MonitorState | null {
  if (!a.monitor) return null;
  if (a.retiredAt != null) {
    const v = verdictRun(a)?.verdict ?? null;
    if (v == null) return "retired";
    return v.outcome === "pass" ? "retired pass" : "retired fail";
  }
  const count = monitor?.infraFailures[a.id] ?? 0;
  if (monitor != null && count >= monitor.brokenThreshold) return "broken";
  if (a.nextRunAt == null) return "paused";
  return "parked";
}

/** Flatten control characters (C0 incl. newlines, DEL, C1) to spaces for a
 *  one-line display string — the verdict note is captured agent output,
 *  untrusted in the panel (the CLI sanitizes the same field with
 *  `sanitize_title`). Delegates to the shared sanitizer in handoff.ts. */
function stripControl(s: string): string {
  return stripControlChars(s, " ");
}

function toRow(
  a: Automation,
  nowMs: number,
  monitor?: MonitorDerivation,
): AutomationRow {
  // Last-run state is *derived* from the history's last row (R25) — no separate
  // mirror to drift, matching the Rust model's `last_run()`.
  const last = a.runs.length > 0 ? a.runs[a.runs.length - 1] : null;
  const lastRunAt = last ? (last.finishedAt ?? last.startedAt) : null;
  // Configured model/effort come from the agent mode; scripts carry neither
  // (U9, R13). The last run's *actual* model/effort come from its RunRow.
  const model = a.mode.kind === "agent" ? a.mode.model : null;
  const effort = a.mode.kind === "agent" ? a.mode.effort : null;
  // The durable verdict record (monitor-handoff R4): outcome/note/bundle come
  // from the newest verdict-bearing row, not necessarily the last run.
  const vRun = a.monitor ? verdictRun(a) : null;
  return {
    id: a.id,
    name: a.name,
    mode: a.mode.kind,
    schedule: humanSchedule(a.cron, a.timezone),
    paused: a.nextRunAt == null,
    retryOnInterrupt: a.retryOnInterrupt,
    nextRun: a.nextRunAt != null ? relativeTime(a.nextRunAt, nowMs) : null,
    lastStatus: last ? last.status : "never run",
    lastRun: lastRunAt != null ? relativeTime(lastRunAt, nowMs) : null,
    lastError: last?.error ?? null,
    linkedPaneId: last?.paneId ?? null,
    model,
    effort,
    lastRunModel: last?.model ?? null,
    lastRunEffort: last?.effort ?? null,
    monitor: a.monitor,
    monitorState: monitorStateOf(a, monitor),
    verdictOutcome: vRun?.verdict?.outcome ?? null,
    verdictNote: vRun?.verdict ? stripControl(vRun.verdict.note) : null,
    bundlePath: vRun?.bundlePath ?? null,
    pickupPointers: a.pickupPointers,
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

// ---- monitor pickup planning (monitor-handoff U7, R16/R17) -------------------
// The pure spawn-vs-fallback decision behind the retired-fail row's pickup
// button. App.svelte performs the two IPCs (the R17 existence check, the
// fallback bundle read) and the tab mutation; everything decidable without an
// app lives here so AE4's one-action guarantee is unit-tested.

/** The R17 existence check's result (ipc `PickupCheck`), or null when the
 *  check IPC itself failed — planPickup treats that as unverifiable and falls
 *  back rather than risking a broken spawn. */
export interface PickupCheckResult {
  transcriptExists: boolean;
  cwdExists: boolean;
}

/**
 * What the pickup button does: exactly one recovery spawn (AE4) — the argv
 * (prompt positional before the variadic `--add-dir`, built by
 * `handoff.ts::buildMonitorPickupCommand`) plus the cwd to spawn it in — or
 * the R17 fallback with a display-ready explanation of why spawning would
 * break. Explanations embed the stored paths control-char-sanitized.
 */
export type PickupPlan =
  | { kind: "spawn"; argv: string[]; cwd: string }
  | { kind: "fallback"; explanation: string };

/**
 * Decide spawn vs fallback for a retired-fail row (R16/R17). Fallback when:
 * no pickup pointers were stored (a legacy or hand-edited record), the
 * existence check couldn't run (`check == null`), or the transcript/cwd no
 * longer exist. Otherwise one spawn plan: the recovery session launches in
 * the parent's cwd with the stock pickup prompt pointing at the bundle and
 * the transcript tail.
 */
export function planPickup(
  row: Pick<AutomationRow, "pickupPointers" | "bundlePath">,
  check: PickupCheckResult | null,
): PickupPlan {
  const ptr = row.pickupPointers;
  if (ptr == null) {
    return {
      kind: "fallback",
      explanation:
        "No pickup pointers were stored for this monitor, so a recovery " +
        "session can't be spawned. The raw failure bundle is shown instead.",
    };
  }
  if (check == null) {
    return {
      kind: "fallback",
      explanation:
        "Couldn't verify the parent session's transcript and directory still " +
        "exist, so no recovery session was spawned. The raw failure bundle " +
        "is shown instead.",
    };
  }
  const missing: string[] = [];
  if (!check.transcriptExists) {
    missing.push(
      `the parent transcript no longer exists (${sanitizeTranscriptPath(ptr.transcriptPath)})`,
    );
  }
  if (!check.cwdExists) {
    missing.push(
      `the session directory no longer exists (${sanitizeTranscriptPath(ptr.sessionCwd)})`,
    );
  }
  if (missing.length > 0) {
    return {
      kind: "fallback",
      explanation:
        `Can't spawn a recovery session: ${missing.join(" and ")}. ` +
        "The raw failure bundle is shown instead.",
    };
  }
  return {
    kind: "spawn",
    argv: buildMonitorPickupCommand(ptr.transcriptPath, row.bundlePath),
    cwd: ptr.sessionCwd,
  };
}

/**
 * Sanitize bundle text for the fallback `<pre>` (R17): strip control
 * characters except newline and tab — the bundle is captured agent output
 * rendered verbatim in the panel, the same untrusted-display posture as the
 * verdict note (multi-line, so newlines survive unlike `stripControl`).
 */
export function sanitizeBundleText(text: string): string {
  return text.replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, "");
}
