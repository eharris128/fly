import { describe, it, expect } from "vitest";
import {
  automationsToRows,
  elapsedShort,
  humanSchedule,
  monitorStateOf,
  planPickup,
  relativeTime,
  sanitizeBundleText,
  type MonitorDerivation,
} from "./automations";
import type {
  Automation,
  AutomationSpec,
  MonitorPointers,
  RunRow,
  RunStatus,
} from "../ipc";

// A minute in ms, and a fixed "now" far enough from 0 that past deltas never
// underflow (matches the CLI rel_label test's stance).
const MIN = 60_000;
const NOW = 1_000_000_000;

function run(status: RunStatus, over: Partial<RunRow> = {}): RunRow {
  return {
    id: "r1",
    mode: "script",
    trigger: "schedule",
    status,
    paneId: null,
    model: null,
    effort: null,
    verdict: null,
    bundlePath: null,
    headless: false,
    output: null,
    exitCode: null,
    error: null,
    scheduledFor: null,
    startedAt: null,
    finishedAt: null,
    ...over,
  };
}

function automation(over: Partial<Automation> = {}): Automation {
  const mode: AutomationSpec = over.mode ?? {
    kind: "script",
    scriptFile: "script",
    interpreter: "bash",
    timeoutMs: 120_000,
  };
  return {
    id: "a1",
    name: "disk watch",
    cron: "*/5 * * * *",
    timezone: "America/New_York",
    enabled: true,
    retryOnInterrupt: false,
    monitor: false,
    notBeforeMs: null,
    retiredAt: null,
    pickupPointers: null,
    cwd: "/tmp",
    origin: { paneId: 7, workspaceId: "ws-1", label: "cli" },
    createdAt: 1_000,
    updatedAt: 1_000,
    nextRunAt: NOW + 5 * MIN,
    runs: [],
    ...over,
    mode,
  };
}

// A parked monitor with pointers, ready for per-test overrides
// (monitor-handoff U7 fixtures).
const POINTERS: MonitorPointers = {
  sessionId: "sess-1",
  transcriptPath: "/home/u/.claude/projects/x/sess-1.jsonl",
  sessionCwd: "/home/u/exp",
};
function monitorAutomation(over: Partial<Automation> = {}): Automation {
  return automation({
    mode: { kind: "agent", prompt: "check it", model: null, effort: null, headless: null },
    monitor: true,
    pickupPointers: POINTERS,
    ...over,
  });
}
// The DTO-derived broken inputs, empty by default (no broken monitors).
function derivation(over: Partial<MonitorDerivation> = {}): MonitorDerivation {
  return { infraFailures: {}, brokenThreshold: 3, ...over };
}

describe("automationsToRows", () => {
  it("sorts by next-run ascending with paused last, ties by name", () => {
    const rows = automationsToRows(
      [
        automation({ id: "p2", name: "zeta", nextRunAt: null }),
        automation({ id: "b", name: "beta", nextRunAt: NOW + 3 * MIN }),
        automation({ id: "a", name: "alpha", nextRunAt: NOW + 1 * MIN }),
        automation({ id: "p1", name: "alpha-paused", nextRunAt: null }),
      ],
      NOW,
    );
    // Scheduled first (by next-run), then paused (by name).
    expect(rows.map((r) => r.id)).toEqual(["a", "b", "p1", "p2"]);
    expect(rows.map((r) => r.paused)).toEqual([false, false, true, true]);
  });

  it("breaks equal next-run ties by name", () => {
    const rows = automationsToRows(
      [
        automation({ id: "y", name: "yankee", nextRunAt: NOW + MIN }),
        automation({ id: "x", name: "xray", nextRunAt: NOW + MIN }),
      ],
      NOW,
    );
    expect(rows.map((r) => r.name)).toEqual(["xray", "yankee"]);
  });

  it("derives last-status / last-run / last-error from the last run row", () => {
    const rows = automationsToRows(
      [
        automation({
          id: "a",
          nextRunAt: NOW + MIN,
          runs: [
            run("succeeded", { id: "old", finishedAt: NOW - 60 * MIN }),
            run("failed", {
              id: "new",
              error: "boom",
              finishedAt: NOW - 5 * MIN,
              paneId: 42,
            }),
          ],
        }),
      ],
      NOW,
    );
    const row = rows[0];
    expect(row.lastStatus).toBe("failed"); // last row wins, not the earlier success
    expect(row.lastRun).toBe("5 minutes ago");
    expect(row.lastError).toBe("boom");
    expect(row.linkedPaneId).toBe(42);
  });

  it("reports never-run automations with a null last-run and no error", () => {
    const rows = automationsToRows([automation({ runs: [] })], NOW);
    expect(rows[0].lastStatus).toBe("never run");
    expect(rows[0].lastRun).toBeNull();
    expect(rows[0].lastError).toBeNull();
    expect(rows[0].linkedPaneId).toBeNull();
  });

  it("carries the agent automation's configured model/effort; scripts carry none (U9)", () => {
    const [agentRow] = automationsToRows(
      [automation({ mode: { kind: "agent", prompt: "x", model: "opus", effort: "high", headless: null } })],
      NOW,
    );
    expect(agentRow.mode).toBe("agent");
    expect(agentRow.model).toBe("opus");
    expect(agentRow.effort).toBe("high");

    // A default script automation carries neither (HomeView renders "—").
    const [scriptRow] = automationsToRows([automation()], NOW);
    expect(scriptRow.mode).toBe("script");
    expect(scriptRow.model).toBeNull();
    expect(scriptRow.effort).toBeNull();

    // An agent with no pinned model → null (HomeView renders "Claude default").
    const [defaultRow] = automationsToRows(
      [automation({ mode: { kind: "agent", prompt: "x", model: null, effort: null, headless: null } })],
      NOW,
    );
    expect(defaultRow.mode).toBe("agent");
    expect(defaultRow.model).toBeNull();
  });

  it("derives the last run's actual model/effort from the last RunRow (U9, R13)", () => {
    const [row] = automationsToRows(
      [
        automation({
          mode: { kind: "agent", prompt: "x", model: "opus", effort: "high", headless: null },
          runs: [run("succeeded", { model: "sonnet", effort: "medium" })],
        }),
      ],
      NOW,
    );
    expect(row.lastRunModel).toBe("sonnet");
    expect(row.lastRunEffort).toBe("medium");
  });

  it("falls back to startedAt when a run has no finishedAt (still running)", () => {
    const rows = automationsToRows(
      [
        automation({
          nextRunAt: null,
          runs: [run("running", { startedAt: NOW - 2 * MIN, finishedAt: null })],
        }),
      ],
      NOW,
    );
    expect(rows[0].lastStatus).toBe("running");
    expect(rows[0].lastRun).toBe("2 minutes ago");
  });

  it("marks a paused automation and nulls its next-run", () => {
    const rows = automationsToRows([automation({ nextRunAt: null })], NOW);
    expect(rows[0].paused).toBe(true);
    expect(rows[0].nextRun).toBeNull();
  });

  it("humanizes next-run for a scheduled automation", () => {
    const rows = automationsToRows([automation({ nextRunAt: NOW + 5 * MIN })], NOW);
    expect(rows[0].nextRun).toBe("in 5 minutes");
    expect(rows[0].schedule).toBe("every 5 min · America/New_York");
    expect(rows[0].mode).toBe("script");
  });

  it("is empty for an empty list", () => {
    expect(automationsToRows([], NOW)).toEqual([]);
  });

  it("leaves recurring automations unaffected by the monitor fields (R18)", () => {
    const [row] = automationsToRows([automation()], NOW, derivation());
    expect(row.monitor).toBe(false);
    expect(row.monitorState).toBeNull();
    expect(row.verdictOutcome).toBeNull();
    expect(row.bundlePath).toBeNull();
    expect(row.pickupPointers).toBeNull();
  });
});

// ---- monitor rows (monitor-handoff U7, R18) ---------------------------------

describe("monitor state derivation", () => {
  it("maps each monitor state to the CLI's exact label", () => {
    // Parked: scheduled, no verdict, count under threshold.
    expect(monitorStateOf(monitorAutomation(), derivation())).toBe("parked");
    // Paused: nextRunAt null, not retired.
    expect(
      monitorStateOf(monitorAutomation({ nextRunAt: null }), derivation()),
    ).toBe("paused");
    // Broken: derived count at the threshold outranks paused/parked.
    expect(
      monitorStateOf(
        monitorAutomation(),
        derivation({ infraFailures: { a1: 3 } }),
      ),
    ).toBe("broken");
    // Retired pass/fail: split by the newest verdict-bearing run row.
    expect(
      monitorStateOf(
        monitorAutomation({
          retiredAt: NOW,
          nextRunAt: null,
          runs: [run("succeeded", { verdict: { outcome: "pass", note: "done" } })],
        }),
        derivation(),
      ),
    ).toBe("retired pass");
    expect(
      monitorStateOf(
        monitorAutomation({
          retiredAt: NOW,
          nextRunAt: null,
          runs: [run("succeeded", { verdict: { outcome: "fail", note: "died" } })],
        }),
        derivation(),
      ),
    ).toBe("retired fail");
    // Defensive: retirement without a surviving verdict row.
    expect(
      monitorStateOf(
        monitorAutomation({ retiredAt: NOW, nextRunAt: null }),
        derivation(),
      ),
    ).toBe("retired");
    // Non-monitor: never a state.
    expect(monitorStateOf(automation(), derivation())).toBeNull();
  });

  it("retired outranks broken; broken outranks paused (CLI precedence)", () => {
    const broken = derivation({ infraFailures: { a1: 5 } });
    expect(
      monitorStateOf(
        monitorAutomation({
          retiredAt: NOW,
          nextRunAt: null,
          runs: [run("succeeded", { verdict: { outcome: "pass", note: "" } })],
        }),
        broken,
      ),
    ).toBe("retired pass");
    expect(monitorStateOf(monitorAutomation({ nextRunAt: null }), broken)).toBe(
      "broken",
    );
  });

  it("sorts parked with recurring by next-run, retired after paused (R18)", () => {
    const rows = automationsToRows(
      [
        monitorAutomation({
          id: "ret",
          name: "aaa-retired",
          retiredAt: NOW,
          nextRunAt: null,
          runs: [run("succeeded", { verdict: { outcome: "fail", note: "x" } })],
        }),
        automation({ id: "pau", name: "bbb-paused", nextRunAt: null }),
        monitorAutomation({ id: "park", name: "zzz-parked", nextRunAt: NOW + MIN }),
        automation({ id: "rec", name: "recurring", nextRunAt: NOW + 2 * MIN }),
      ],
      NOW,
      derivation(),
    );
    // Parked monitor rides with recurring by next-run (1 min < 2 min), then
    // paused, then retired last — despite the retired row's first-place name.
    expect(rows.map((r) => r.id)).toEqual(["park", "rec", "pau", "ret"]);
  });

  it("carries the verdict, bundle path, and pointers on the row; sanitizes the note", () => {
    const [row] = automationsToRows(
      [
        monitorAutomation({
          retiredAt: NOW,
          nextRunAt: null,
          runs: [
            run("succeeded", {
              verdict: { outcome: "fail", note: "line1\nline2\u0007" },
              bundlePath: "/data/monitor-bundles/a1-r1.md",
            }),
          ],
        }),
      ],
      NOW,
      derivation(),
    );
    expect(row.monitorState).toBe("retired fail");
    expect(row.verdictOutcome).toBe("fail");
    expect(row.verdictNote).toBe("line1 line2 "); // control chars flattened
    expect(row.bundlePath).toBe("/data/monitor-bundles/a1-r1.md");
    expect(row.pickupPointers).toEqual(POINTERS);
  });

  it("reads the newest verdict-bearing run, not the last row", () => {
    const [row] = automationsToRows(
      [
        monitorAutomation({
          retiredAt: NOW,
          nextRunAt: null,
          runs: [
            run("succeeded", {
              id: "v",
              verdict: { outcome: "fail", note: "died" },
              bundlePath: "/b/a1-v.md",
            }),
            run("skipped", { id: "later" }), // e.g. a refused post-retire claim
          ],
        }),
      ],
      NOW,
      derivation(),
    );
    expect(row.verdictOutcome).toBe("fail");
    expect(row.bundlePath).toBe("/b/a1-v.md");
  });

  it("derives no broken state without the DTO inputs (legacy caller)", () => {
    expect(monitorStateOf(monitorAutomation())).toBe("parked");
  });
});

// ---- pickup planning (monitor-handoff U7, R16/R17) --------------------------

describe("planPickup", () => {
  const row = { pickupPointers: POINTERS, bundlePath: "/data/monitor-bundles/a1-r1.md" };
  const ok = { transcriptExists: true, cwdExists: true };

  it("spawns exactly one recovery session when everything resolves (AE4/R16)", () => {
    const plan = planPickup(row, ok);
    expect(plan.kind).toBe("spawn");
    if (plan.kind !== "spawn") return;
    expect(plan.cwd).toBe("/home/u/exp");
    // The argv: prompt positional BEFORE the variadic --add-dir, no bypass flag.
    expect(plan.argv[0]).toBe("claude");
    expect(plan.argv[1]).toContain("/data/monitor-bundles/a1-r1.md");
    expect(plan.argv[1]).toContain(POINTERS.transcriptPath);
    expect(plan.argv.indexOf("--add-dir")).toBeGreaterThan(
      plan.argv.findIndex((a) => a.includes("failure bundle")),
    );
    expect(plan.argv).not.toContain("--dangerously-skip-permissions");
  });

  it("falls back when the transcript is gone, naming the path (R17)", () => {
    const plan = planPickup(row, { transcriptExists: false, cwdExists: true });
    expect(plan.kind).toBe("fallback");
    if (plan.kind !== "fallback") return;
    expect(plan.explanation).toContain("transcript no longer exists");
    expect(plan.explanation).toContain(POINTERS.transcriptPath);
  });

  it("falls back when the cwd is gone, and when both are gone (R17)", () => {
    const cwdGone = planPickup(row, { transcriptExists: true, cwdExists: false });
    expect(cwdGone.kind).toBe("fallback");
    if (cwdGone.kind === "fallback")
      expect(cwdGone.explanation).toContain("directory no longer exists");
    const bothGone = planPickup(row, { transcriptExists: false, cwdExists: false });
    expect(bothGone.kind).toBe("fallback");
    if (bothGone.kind === "fallback")
      expect(bothGone.explanation).toContain(" and ");
  });

  it("falls back when there are no pointers or the check itself failed", () => {
    expect(planPickup({ pickupPointers: null, bundlePath: null }, ok).kind).toBe(
      "fallback",
    );
    expect(planPickup(row, null).kind).toBe("fallback");
  });

  it("sanitizes control characters out of paths embedded in explanations", () => {
    const evil = {
      pickupPointers: {
        ...POINTERS,
        transcriptPath: "/tmp/x\u001b[31m.jsonl",
      },
      bundlePath: null,
    };
    const plan = planPickup(evil, { transcriptExists: false, cwdExists: true });
    if (plan.kind === "fallback") expect(plan.explanation).not.toContain("\u001b");
  });
});

describe("sanitizeBundleText", () => {
  it("strips control characters but keeps newlines and tabs", () => {
    expect(sanitizeBundleText("a\u0007b\u001b[31mc\nd\te\u009f")).toBe(
      "ab[31mc\nd\te",
    );
  });
});

describe("humanSchedule", () => {
  it("recognizes common cron shapes and appends the timezone", () => {
    expect(humanSchedule("*/5 * * * *", "UTC")).toBe("every 5 min · UTC");
    expect(humanSchedule("* * * * *", "UTC")).toBe("every minute · UTC");
    expect(humanSchedule("*/1 * * * *", "UTC")).toBe("every minute · UTC");
    expect(humanSchedule("30 * * * *", "UTC")).toBe("hourly · UTC");
    expect(humanSchedule("0 9 * * *", "America/New_York")).toBe(
      "daily · America/New_York",
    );
    expect(humanSchedule("0 9 * * 1", "UTC")).toBe("weekly · UTC");
    expect(humanSchedule("0 9 1 * *", "UTC")).toBe("monthly · UTC");
  });

  it("falls back to the raw cron for shapes it does not recognize", () => {
    expect(humanSchedule("15,45 9-17 * * *", "UTC")).toBe("15,45 9-17 * * * · UTC");
    // Not a clean 5-field expression → passes through verbatim.
    expect(humanSchedule("bogus", "UTC")).toBe("bogus · UTC");
  });
});

describe("relativeTime", () => {
  it("reads 'just now' within 45 seconds either side", () => {
    expect(relativeTime(NOW, NOW)).toBe("just now");
    expect(relativeTime(NOW + 44_000, NOW)).toBe("just now");
    expect(relativeTime(NOW - 44_000, NOW)).toBe("just now");
  });

  it("formats future times with 'in' and pluralizes", () => {
    expect(relativeTime(NOW + 5 * MIN, NOW)).toBe("in 5 minutes");
    expect(relativeTime(NOW + 1 * MIN, NOW)).toBe("in 1 minute");
    expect(relativeTime(NOW + 2 * 60 * MIN, NOW)).toBe("in 2 hours");
    expect(relativeTime(NOW + 3 * 24 * 60 * MIN, NOW)).toBe("in 3 days");
  });

  it("formats past times with 'ago' and pluralizes", () => {
    expect(relativeTime(NOW - 5 * MIN, NOW)).toBe("5 minutes ago");
    expect(relativeTime(NOW - 1 * 60 * MIN, NOW)).toBe("1 hour ago");
    expect(relativeTime(NOW - 1 * 24 * 60 * MIN, NOW)).toBe("1 day ago");
  });
});

// Headless-agent-automations U4 (R9): the row's effective disposition mirrors
// the claim's resolution — monitor forces, explicit pin wins, else the DTO's
// config default — and a still-running last run carries a live elapsed read.
describe("headless disposition & running elapsed", () => {
  const agent = (headless: boolean | null) =>
    automation({
      mode: { kind: "agent", prompt: "x", model: null, effort: null, headless },
    });

  it("resolves monitor > explicit pin > config default; scripts never", () => {
    expect(automationsToRows([agent(null)], NOW)[0].headless).toBe(true); // shipped default
    expect(
      automationsToRows([agent(null)], NOW, undefined, false)[0].headless,
    ).toBe(false);
    expect(
      automationsToRows([agent(true)], NOW, undefined, false)[0].headless,
    ).toBe(true);
    expect(
      automationsToRows([agent(false)], NOW, undefined, true)[0].headless,
    ).toBe(false);
    expect(
      automationsToRows([monitorAutomation()], NOW, undefined, false)[0].headless,
    ).toBe(true);
    expect(
      automationsToRows([automation()], NOW, undefined, true)[0].headless,
    ).toBe(false); // script
  });

  it("renders a live running read with elapsed time and none for terminal rows", () => {
    const running = automation({
      runs: [run("running", { startedAt: NOW - 2 * MIN, finishedAt: null })],
    });
    const [row] = automationsToRows([running], NOW);
    expect(row.lastStatus).toBe("running");
    expect(row.runningFor).toBe("2m");

    const done = automation({
      runs: [run("succeeded", { startedAt: NOW - 9 * MIN, finishedAt: NOW - MIN })],
    });
    expect(automationsToRows([done], NOW)[0].runningFor).toBeNull();
    expect(automationsToRows([automation({ runs: [] })], NOW)[0].runningFor).toBeNull();
  });

  it("elapsedShort coarsens honestly", () => {
    expect(elapsedShort(10_000)).toBe("<1m");
    expect(elapsedShort(-5_000)).toBe("<1m");
    expect(elapsedShort(12 * MIN)).toBe("12m");
    expect(elapsedShort(65 * MIN)).toBe("1h 05m");
    expect(elapsedShort(150 * MIN)).toBe("2h 30m");
  });
});
