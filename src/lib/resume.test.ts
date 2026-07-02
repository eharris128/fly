import { describe, it, expect } from "vitest";
import {
  buildResumeCommand,
  resumeCommandsForLeaves,
  shouldCaptureSession,
  resumeStaleVerdict,
  classifyResumeTier,
  resumeLeafDecision,
  resumeTierSummary,
  planResumeLeaves,
  resumeOfferBreakdown,
  resumeNoticeText,
} from "./resume";
import type { ResumeRecord } from "../ipc";

// The configured flag floor (R8): replayed when no argv was captured.
const DEFAULT = ["--dangerously-skip-permissions"];

function rec(over: Partial<ResumeRecord>): ResumeRecord {
  return {
    sessionId: null,
    sessionCwd: null,
    argv: null,
    isAgent: false,
    updatedAt: 0,
    ...over,
  };
}

describe("buildResumeCommand (U5)", () => {
  it("returns null for no record → bare shell", () => {
    expect(buildResumeCommand(null, DEFAULT)).toBeNull();
    expect(buildResumeCommand(undefined, DEFAULT)).toBeNull();
  });

  it("appends --resume <id> after replayed flags, preserving the skip flag", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--dangerously-skip-permissions"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--dangerously-skip-permissions", "--resume", "x"]);
  });

  it("preserves a --model value flag", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--model", "opus", "--dangerously-skip-permissions"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual([
      "claude",
      "--model",
      "opus",
      "--dangerously-skip-permissions",
      "--resume",
      "x",
    ]);
  });

  it("falls back to the flag floor with --resume <id> when no argv was captured", () => {
    const out = buildResumeCommand(rec({ sessionId: "abc", isAgent: true }), DEFAULT);
    expect(out).toEqual(["claude", "--resume", "abc", "--dangerously-skip-permissions"]);
  });

  it("uses --continue + flag floor for an agent with neither id nor argv", () => {
    const out = buildResumeCommand(rec({ isAgent: true }), DEFAULT);
    expect(out).toEqual(["claude", "--continue", "--dangerously-skip-permissions"]);
  });

  it("strips an existing --continue and appends --resume <id>", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--continue"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--resume", "x"]);
  });

  it("strips a pre-existing --resume and its id value", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "-r", "stale-id", "--model", "opus"], sessionId: "new" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--model", "opus", "--resume", "new"]);
  });

  it("strips the --resume=<id> form", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--resume=stale"], sessionId: "new" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--resume", "new"]);
  });

  it("strips a trailing positional prompt after a boolean flag (no re-send)", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--dangerously-skip-permissions", "write a poem"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--dangerously-skip-permissions", "--resume", "x"]);
  });

  it("strips a trailing prompt but keeps a value-flag's value", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--model", "opus", "explain this code"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--model", "opus", "--resume", "x"]);
  });

  it("strips a mid-argv prompt — a resumed handoff pane must not re-fire it", () => {
    // The quick-handoff argv puts the prompt BEFORE --add-dir (the flag is
    // variadic and would swallow a trailing positional; session-handoff U2/U4,
    // KTD3). Positionals are dropped wherever they sit.
    const out = buildResumeCommand(
      rec({
        argv: ["claude", "pick up the previous session", "--add-dir", "/home/u/.claude/projects/-p"],
        sessionId: "x",
      }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--add-dir", "/home/u/.claude/projects/-p", "--resume", "x"]);
  });

  it("keeps every value of a variadic --add-dir, still dropping a trailing prompt", () => {
    // `--add-dir <directories...>` is variadic in the claude CLI: a user-launched
    // pane with two added dirs must resume with BOTH, not `--add-dir /a` with
    // `/b` dropped as a bare positional. The multi-word trailing prompt is still
    // stripped (the VARIADIC_FLAGS whitespace stop rule), never re-fired.
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--add-dir", "/a", "/b", "fix the bug"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--add-dir", "/a", "/b", "--resume", "x"]);
  });

  it("preserves a node-wrapper argv[0..2] and appends --resume <id>", () => {
    const out = buildResumeCommand(
      rec({
        argv: ["node", "/home/u/.npm/lib/node_modules/claude/cli.js", "--dangerously-skip-permissions"],
        sessionId: "x",
      }),
      DEFAULT,
    );
    expect(out).toEqual([
      "node",
      "/home/u/.npm/lib/node_modules/claude/cli.js",
      "--dangerously-skip-permissions",
      "--resume",
      "x",
    ]);
  });

  it("uses --continue with replayed flags when argv is present but no id", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--model", "opus"], isAgent: true }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--model", "opus", "--continue"]);
  });

  it("treats an empty argv as no argv (uses the floor)", () => {
    const out = buildResumeCommand(rec({ argv: [], sessionId: "x" }), DEFAULT);
    expect(out).toEqual(["claude", "--resume", "x", "--dangerously-skip-permissions"]);
  });
});

describe("shouldCaptureSession (fix-003 U2, KTD-B)", () => {
  it("captures the first time an id is seen (lastSeen null)", () => {
    expect(shouldCaptureSession(null, "sess-A")).toBe(true);
  });

  it("skips when the id is unchanged", () => {
    expect(shouldCaptureSession("sess-A", "sess-A")).toBe(false);
  });

  it("captures when the id changes (/clear, new conversation)", () => {
    expect(shouldCaptureSession("sess-A", "sess-B")).toBe(true);
  });

  it("skips a null resolution, preserving the last captured id", () => {
    // A transient miss (no active transcript) must not clear what we have.
    expect(shouldCaptureSession("sess-A", null)).toBe(false);
    expect(shouldCaptureSession(null, null)).toBe(false);
  });
});

describe("resumeStaleVerdict (fix-003 U3, KTD-C)", () => {
  const MARGIN = 60_000;

  it("is stale when the candidate's last turn predates pane activity − margin", () => {
    expect(
      resumeStaleVerdict({
        candidateLastTurnMs: 1_000_000,
        paneActivityMs: 5_000_000,
        marginMs: MARGIN,
      }),
    ).toBe("stale");
  });

  it("is fresh when the candidate's last turn is within/after pane activity", () => {
    // Equal-to-activity → fresh; slightly-before but within margin → fresh.
    expect(
      resumeStaleVerdict({
        candidateLastTurnMs: 5_000_000,
        paneActivityMs: 5_000_000,
        marginMs: MARGIN,
      }),
    ).toBe("fresh");
    expect(
      resumeStaleVerdict({
        candidateLastTurnMs: 5_000_000 - 30_000,
        paneActivityMs: 5_000_000,
        marginMs: MARGIN,
      }),
    ).toBe("fresh");
  });

  it("treats a missing candidate timestamp as stale (prefer a clean shell)", () => {
    expect(
      resumeStaleVerdict({
        candidateLastTurnMs: null,
        paneActivityMs: 5_000_000,
        marginMs: MARGIN,
      }),
    ).toBe("stale");
  });

  it("the AE1/AE3 fixture: a 06-19 last turn vs a 06-23 pane activity is stale", () => {
    // The reported bug, as the guard sees it: a metadata-only --continue open
    // bumped the file's mtime to 06-23, but the last real turn is 06-19, before
    // the pane's own captured activity → stale → bare shell, not resurrection.
    const lastTurn0619 = Date.parse("2026-06-19T19:17:16.402Z");
    const paneActivity0623 = Date.parse("2026-06-23T17:36:00.000Z");
    expect(
      resumeStaleVerdict({
        candidateLastTurnMs: lastTurn0619,
        paneActivityMs: paneActivity0623,
        marginMs: MARGIN,
      }),
    ).toBe("stale");
  });
});

describe("classifyResumeTier (fix-003 U3/U4)", () => {
  it("is precise when the record carries a session id", () => {
    expect(classifyResumeTier(rec({ sessionId: "x", isAgent: true }))).toBe("precise");
  });

  it("is imprecise without a session id", () => {
    expect(classifyResumeTier(rec({ isAgent: true }))).toBe("imprecise");
    expect(classifyResumeTier(rec({ argv: ["claude"], isAgent: true }))).toBe("imprecise");
  });

  it("is imprecise for a null/undefined record", () => {
    expect(classifyResumeTier(null)).toBe("imprecise");
    expect(classifyResumeTier(undefined)).toBe("imprecise");
  });
});

describe("resumeLeafDecision (fix-003 U3 keep/drop)", () => {
  const MARGIN = 60_000;

  it("keeps a precise leaf and bypasses the guard (candidate irrelevant)", () => {
    // A precise leaf keeps even if the (ignored) candidate would look stale.
    expect(
      resumeLeafDecision(rec({ sessionId: "x", updatedAt: 9_000_000 }), 1_000, MARGIN),
    ).toEqual({ tier: "precise", keep: true });
  });

  it("keeps a fresh imprecise leaf (--continue candidate within pane activity)", () => {
    expect(
      resumeLeafDecision(
        rec({ isAgent: true, updatedAt: 5_000_000 }),
        5_000_000,
        MARGIN,
      ),
    ).toEqual({ tier: "imprecise", keep: true });
  });

  it("drops a stale imprecise leaf (candidate predates pane activity)", () => {
    // The play bug: a 06-19 candidate vs a 06-23 pane → imprecise, dropped.
    expect(
      resumeLeafDecision(
        rec({ isAgent: true, updatedAt: Date.parse("2026-06-23T17:36:00.000Z") }),
        Date.parse("2026-06-19T19:17:16.402Z"),
        MARGIN,
      ),
    ).toEqual({ tier: "imprecise", keep: false });
  });

  it("drops an imprecise leaf with no candidate (no --continue target)", () => {
    expect(
      resumeLeafDecision(rec({ isAgent: true, updatedAt: 5_000_000 }), null, MARGIN),
    ).toEqual({ tier: "imprecise", keep: false });
  });
});

describe("planResumeLeaves (fix-003 U3 — the orchestration core)", () => {
  const MARGIN = 60_000;
  // The plan's U3 "Logic" scenario: keep a precise leaf, keep a fresh imprecise
  // leaf as --continue, drop a stale one — all in one fixture.
  const records = {
    "leaf-precise": rec({ sessionId: "sess-x", isAgent: true }),
    "leaf-fresh": rec({ isAgent: true, updatedAt: 5_000_000 }),
    "leaf-stale": rec({ isAgent: true, updatedAt: 5_000_000 }),
  };
  const commands = {
    "leaf-precise": ["claude", "--resume", "sess-x"],
    "leaf-fresh": ["claude", "--continue"],
    "leaf-stale": ["claude", "--continue"],
  };
  const candidates = {
    "leaf-fresh": 5_000_000, // last turn == pane activity → fresh
    "leaf-stale": 1_000_000, // last turn well before pane activity → stale
    // leaf-precise needs no candidate (bypasses the guard)
  };

  it("keeps precise + fresh imprecise, drops stale, counts the drop", () => {
    const out = planResumeLeaves(commands, records, candidates, MARGIN);
    expect(out.commands).toEqual({
      "leaf-precise": ["claude", "--resume", "sess-x"],
      "leaf-fresh": ["claude", "--continue"],
    });
    expect(out.tierByLeaf).toEqual({
      "leaf-precise": "precise",
      "leaf-fresh": "imprecise",
    });
    expect(out.staleDropped).toBe(1);
    expect(out.commands["leaf-stale"]).toBeUndefined(); // → bare shell
  });

  it("a precise leaf keeps even with a missing candidate (guard bypassed)", () => {
    const out = planResumeLeaves(
      { "leaf-precise": ["claude", "--resume", "sess-x"] },
      { "leaf-precise": rec({ sessionId: "sess-x" }) },
      {}, // no candidate fetched for a precise leaf
      MARGIN,
    );
    expect(out.tierByLeaf).toEqual({ "leaf-precise": "precise" });
    expect(out.staleDropped).toBe(0);
  });

  it("drops every imprecise leaf when all candidates are stale/missing", () => {
    const out = planResumeLeaves(
      { a: ["claude", "--continue"], b: ["claude", "--continue"] },
      { a: rec({ isAgent: true, updatedAt: 9_000_000 }), b: rec({ isAgent: true, updatedAt: 9_000_000 }) },
      { a: 1_000, b: null },
      MARGIN,
    );
    expect(out.commands).toEqual({});
    expect(out.staleDropped).toBe(2);
  });
});

describe("resumeOfferBreakdown (fix-003 U4)", () => {
  it("is null when every resumable leaf is exact", () => {
    expect(resumeOfferBreakdown({ precise: 3, imprecise: 0 }, 0)).toBeNull();
  });

  it("lists exact + most-recent-in-folder + stale", () => {
    expect(resumeOfferBreakdown({ precise: 2, imprecise: 1 }, 1)).toBe(
      "2 exact · 1 most-recent-in-folder · 1 stale, started fresh",
    );
  });

  it("shows just the imprecise part when there are no exact or stale", () => {
    expect(resumeOfferBreakdown({ precise: 0, imprecise: 2 }, 0)).toBe(
      "2 most-recent-in-folder",
    );
  });

  it("shows a stale-only breakdown even with no imprecise resumes", () => {
    expect(resumeOfferBreakdown({ precise: 1, imprecise: 0 }, 2)).toBe(
      "1 exact · 2 stale, started fresh",
    );
  });
});

describe("resumeNoticeText (fix-003 U4)", () => {
  it("is null when everything resumed exactly", () => {
    expect(resumeNoticeText({ precise: 2, imprecise: 0 }, 0)).toBeNull();
  });

  it("names the imprecise resumes (with plural agreement)", () => {
    expect(resumeNoticeText({ precise: 1, imprecise: 1 }, 0)).toBe(
      "1 agent resumed by most-recent session in folder.",
    );
    expect(resumeNoticeText({ precise: 0, imprecise: 2 }, 0)).toBe(
      "2 agents resumed by most-recent session in folder.",
    );
  });

  it("names stale-dropped agents (the AE3 disclosure)", () => {
    expect(resumeNoticeText({ precise: 0, imprecise: 0 }, 1)).toBe(
      "1 agent started fresh — no recent session to resume.",
    );
  });

  it("joins imprecise and stale clauses", () => {
    expect(resumeNoticeText({ precise: 1, imprecise: 1 }, 2)).toBe(
      "1 agent resumed by most-recent session in folder; 2 agents started fresh — no recent session to resume.",
    );
  });
});

describe("resumeTierSummary (fix-003 U4)", () => {
  it("counts a mixed map into precise/imprecise", () => {
    expect(
      resumeTierSummary({
        "leaf-1": "precise",
        "leaf-2": "precise",
        "leaf-3": "imprecise",
      }),
    ).toEqual({ precise: 2, imprecise: 1 });
  });

  it("is zeros for an empty map", () => {
    expect(resumeTierSummary({})).toEqual({ precise: 0, imprecise: 0 });
  });

  it("an all-precise map has no imprecise count (no degraded labeling)", () => {
    expect(resumeTierSummary({ a: "precise", b: "precise" })).toEqual({
      precise: 2,
      imprecise: 0,
    });
  });
});

describe("resumeCommandsForLeaves (U8)", () => {
  it("maps each restored leaf to its command, omitting bare-shell leaves", () => {
    const records = {
      "leaf-1": rec({ argv: ["claude", "--model", "opus"], sessionId: "a" }),
      "leaf-2": rec({ sessionId: "b", isAgent: true }), // no argv → floor
      // leaf-3 has no record → bare shell, omitted from the result
    };
    const out = resumeCommandsForLeaves(["leaf-1", "leaf-2", "leaf-3"], records, DEFAULT);
    expect(out).toEqual({
      "leaf-1": ["claude", "--model", "opus", "--resume", "a"],
      "leaf-2": ["claude", "--resume", "b", "--dangerously-skip-permissions"],
    });
    expect(out["leaf-3"]).toBeUndefined();
  });

  it("ignores orphan records whose leaf isn't in the layout", () => {
    const records = {
      "leaf-1": rec({ sessionId: "a", isAgent: true }),
      "gone-leaf": rec({ sessionId: "z", isAgent: true }),
    };
    const out = resumeCommandsForLeaves(["leaf-1"], records, DEFAULT);
    expect(Object.keys(out)).toEqual(["leaf-1"]);
  });
});
