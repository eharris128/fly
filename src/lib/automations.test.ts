import { describe, it, expect } from "vitest";
import { automationsToRows, humanSchedule, relativeTime } from "./automations";
import type { Automation, AutomationSpec, RunRow, RunStatus } from "../ipc";

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
