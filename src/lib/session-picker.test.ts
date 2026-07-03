import { describe, it, expect } from "vitest";
import {
  candidatesToRows,
  sortCandidates,
  clampIndex,
  pickerPlan,
  takesPrecisePath,
  shouldForceSessionPick,
  provenanceLabel,
  NO_SNIPPET_FALLBACK,
} from "./session-picker";
import type { HandoffCandidate } from "../ipc";

const NOW = 1_800_000_000_000;

function cand(over: Partial<HandoffCandidate>): HandoffCandidate {
  return {
    sessionId: "aaaabbbb-cccc-dddd-eeee-ffff00001111",
    transcriptPath: "/root/-proj-app/x.jsonl",
    sessionCwd: "/proj/app",
    lastTurnMs: NOW - 60_000,
    sessionSource: "pick",
    divergencePending: false,
    snippet: "fix the picker",
    ...over,
  };
}

describe("candidatesToRows (U6, R7)", () => {
  it("renders rows in last-activity order regardless of input order", () => {
    const sorted = sortCandidates([
      cand({ sessionId: "old-session", lastTurnMs: NOW - 3_600_000 }),
      cand({ sessionId: "new-session", lastTurnMs: NOW - 60_000 }),
      cand({ sessionId: "mid-session", lastTurnMs: NOW - 600_000 }),
    ]);
    const rows = candidatesToRows(sorted, NOW);
    expect(rows.map((r) => r.shortId)).toEqual([
      "new-sess",
      "mid-sess",
      "old-sess",
    ]);
    // Index-aligned with the sorted candidates, so a picked row maps back.
    expect(sorted[0].sessionId).toBe("new-session");
    expect(rows[0].when).toBe("1m ago");
    expect(rows[2].when).toBe("1h ago");
  });

  it("empty input yields empty rows", () => {
    expect(candidatesToRows([], NOW)).toEqual([]);
  });

  it("falls back to an explicit label when a candidate has no snippet", () => {
    const rows = candidatesToRows([cand({ snippet: null })], NOW);
    expect(rows[0].snippet).toBe(NO_SNIPPET_FALLBACK);
  });
});

describe("clampIndex", () => {
  it("clamps a stranded selection into range", () => {
    expect(clampIndex(5, 3)).toBe(2);
    expect(clampIndex(-1, 3)).toBe(0);
    expect(clampIndex(1, 3)).toBe(1);
    expect(clampIndex(0, 0)).toBe(0);
  });
});

describe("pickerPlan (R6/R9/R11)", () => {
  it("no candidates routes to the notice, never an empty picker", () => {
    expect(pickerPlan(0, false)).toBe("notice");
    expect(pickerPlan(0, true)).toBe("notice");
  });

  it("exactly one candidate stays zero-prompt (R9)", () => {
    expect(pickerPlan(1, false)).toBe("auto");
  });

  it("two or more candidates show the list (R6)", () => {
    expect(pickerPlan(2, false)).toBe("list");
    expect(pickerPlan(7, false)).toBe("list");
  });

  it("a forced re-pick lists even a single candidate", () => {
    // A divergence re-pick / force re-pick exists to re-examine a suspect
    // binding — auto-proceeding would defeat it (KTD2/KTD7).
    expect(pickerPlan(1, true)).toBe("list");
  });
});

describe("handoff routing predicates (AE1/AE6, R14/KTD2 + corroborate-then-remember)", () => {
  it("takesPrecisePath, quick: only an explicit Pick authorizes the zero-prompt bypass path", () => {
    // The corroboration gate (the plan's bypass-permissions Open-Question
    // resolution): a forgeable Hook capture or a Poll guess never fires the
    // unattended --dangerously-skip-permissions spawn without a one-time pick.
    expect(takesPrecisePath({ divergencePending: false, sessionSource: "pick" }, "quick")).toBe(true);
    expect(takesPrecisePath({ divergencePending: false, sessionSource: "hook" }, "quick")).toBe(false);
    expect(takesPrecisePath({ divergencePending: false, sessionSource: "poll" }, "quick")).toBe(false);
    expect(takesPrecisePath(null, "quick")).toBe(false);
  });

  it("takesPrecisePath, guided: any non-divergent resolved target proceeds (in-loop mode)", () => {
    expect(takesPrecisePath({ divergencePending: false, sessionSource: "hook" }, "guided")).toBe(true);
    expect(takesPrecisePath({ divergencePending: false, sessionSource: "poll" }, "guided")).toBe(true);
    expect(takesPrecisePath({ divergencePending: false, sessionSource: "pick" }, "guided")).toBe(true);
    expect(takesPrecisePath(null, "guided")).toBe(false);
  });

  it("takesPrecisePath: divergence blocks the precise path in both modes, even for a Pick", () => {
    expect(takesPrecisePath({ divergencePending: true, sessionSource: "pick" }, "quick")).toBe(false);
    expect(takesPrecisePath({ divergencePending: true, sessionSource: "hook" }, "guided")).toBe(false);
  });

  it("shouldForceSessionPick: divergence forces the list even unforced (guided)", () => {
    // A divergence-pending target must re-examine the binding (AE6) regardless
    // of the forcePick flag — pinned on guided, where nothing else forces.
    expect(shouldForceSessionPick({ divergencePending: true }, false, "guided")).toBe(true);
  });

  it("shouldForceSessionPick: forcePick forces the list with a null target (guided)", () => {
    // U8 force re-pick skips resolution (null target) but must still list.
    expect(shouldForceSessionPick(null, true, "guided")).toBe(true);
  });

  it("shouldForceSessionPick: quick always forces — no R9 auto for a bypass spawn", () => {
    // Corroborate-then-remember: a lone (possibly planted) transcript must not
    // auto-launch an unattended bypass-permissions agent; one explicit pick is
    // required, then remembered.
    expect(shouldForceSessionPick(null, false, "quick")).toBe(true);
    expect(shouldForceSessionPick({ divergencePending: false }, false, "quick")).toBe(true);
  });

  it("shouldForceSessionPick: guided, non-divergent, unforced keeps R9's auto path", () => {
    expect(shouldForceSessionPick({ divergencePending: false }, false, "guided")).toBe(false);
    expect(shouldForceSessionPick(null, false, "guided")).toBe(false);
  });
});

describe("provenanceLabel (KTD4)", () => {
  it("names all three sources distinctly", () => {
    const labels = new Set([
      provenanceLabel("pick"),
      provenanceLabel("hook"),
      provenanceLabel("poll"),
    ]);
    expect(labels.size).toBe(3);
    expect(provenanceLabel("pick")).toContain("pick");
  });
});
