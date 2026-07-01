import { describe, it, expect } from "vitest";
import {
  buildHomeModel,
  effectiveAttention,
  effectiveTaskCount,
  formatDuration,
  formatTaskCount,
  agentCount,
  agentJumpTarget,
  firstRaised,
  usageLimitLabel,
  formatResetTime,
} from "./home";
import type { Tab, Workspace } from "./workspaces";
import type { Node } from "./layout";
import type { AttentionReason, PaneActivity } from "../ipc";

const leaf = (key: string): Node => ({ kind: "leaf", key });
const split = (key: string, first: Node, second: Node): Node => ({
  kind: "split",
  key,
  orientation: "horizontal",
  ratio: 0.5,
  first,
  second,
});

function tab(id: string, tree: Node, focusedLeafKey: string, title: string | null = null): Tab {
  return { id, tree, focusedLeafKey, title };
}
function ws(id: string, name: string, tabs: Tab[]): Workspace {
  return { id, name, tabs, activeTabId: tabs[0]?.id ?? "" };
}
function agent(
  isAgent: boolean,
  workingForMs: number | null = null,
  liveTaskCount = 0,
): PaneActivity {
  return { isAgent, workingForMs, lastOutputAgoMs: workingForMs, liveTaskCount };
}

describe("buildHomeModel", () => {
  it("groups agent leaves under their workspace and tab", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [
        tab("tab-1", leaf("leaf-1"), "leaf-1"),
        tab("tab-2", leaf("leaf-2"), "leaf-2"),
      ]),
    ];
    const model = buildHomeModel(
      workspaces,
      { "leaf-1": agent(true, 5000), "leaf-2": agent(false) },
      { "leaf-1": "/home/u/proj" },
      {},
    );
    expect(model).toHaveLength(1);
    expect(model[0].wsId).toBe("ws-1");
    expect(model[0].name).toBe("Alpha");
    // tab-2 (non-agent) is dropped entirely.
    expect(model[0].tabs).toHaveLength(1);
    expect(model[0].tabs[0].tabId).toBe("tab-1");
    expect(model[0].tabs[0].title).toBe("proj");
    expect(model[0].tabs[0].rows).toHaveLength(1);
    const row = model[0].tabs[0].rows[0];
    expect(row).toMatchObject({
      leafKey: "leaf-1",
      cwd: "/home/u/proj",
      workingForMs: 5000,
      needsAttention: false,
      status: "working",
    });
  });

  it("excludes non-agent leaves and omits empty tabs/workspaces", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [tab("tab-1", leaf("leaf-1"), "leaf-1")]),
      ws("ws-2", "Beta", [tab("tab-2", leaf("leaf-2"), "leaf-2")]),
    ];
    const model = buildHomeModel(
      workspaces,
      { "leaf-1": agent(false), "leaf-2": agent(false) },
      {},
      {},
    );
    expect(model).toEqual([]); // no agents anywhere → empty (R7)
    expect(agentCount(model)).toBe(0);
  });

  it("applies the status precedence: raised=waiting, acknowledged=idle, else stretch", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [
        tab(
          "tab-1",
          split("s-1", leaf("a"), split("s-2", leaf("b"), split("s-3", leaf("c"), leaf("d")))),
          "a",
        ),
      ]),
    ];
    const model = buildHomeModel(
      workspaces,
      {
        a: agent(true, 9000), // raised → waiting despite a stretch
        b: agent(true, 3000), // idle attention + stretch → working
        c: agent(true, null), // idle attention + no stretch → idle
        d: agent(true, 5000), // acknowledged (you're viewing) → idle, not working/waiting
      },
      {},
      { a: "raised", d: "acknowledged" },
    );
    const rows = model[0].tabs[0].rows;
    expect(rows.map((r) => [r.leafKey, r.status])).toEqual([
      ["a", "waiting"],
      ["b", "working"],
      ["c", "idle"],
      ["d", "idle"],
    ]);
    // raised is urgent needs-you; acknowledged (already seen) is not.
    expect(rows[0].needsAttention).toBe(true);
    expect(rows[3].needsAttention).toBe(false);
    expect(rows[0].workingForMs).toBe(9000);
  });

  it("treats acknowledged as parked (idle), not waiting, even with a stretch", () => {
    const workspaces = [ws("ws-1", "Alpha", [tab("tab-1", leaf("leaf-1"), "leaf-1")])];
    const model = buildHomeModel(
      workspaces,
      { "leaf-1": agent(true, 4000) }, // a lingering stretch...
      {},
      { "leaf-1": "acknowledged" }, // ...but you're viewing it → neither working nor waiting
    );
    const row = model[0].tabs[0].rows[0];
    expect(row.status).toBe("idle");
    expect(row.needsAttention).toBe(false);
  });

  it("carries workingForMs through, null when idle", () => {
    const workspaces = [ws("ws-1", "Alpha", [tab("tab-1", leaf("leaf-1"), "leaf-1")])];
    const idle = buildHomeModel(workspaces, { "leaf-1": agent(true, null) }, {}, {});
    expect(idle[0].tabs[0].rows[0].workingForMs).toBeNull();
    expect(idle[0].tabs[0].rows[0].status).toBe("idle");
  });

  it("returns [] for empty inputs", () => {
    expect(buildHomeModel([], {}, {}, {})).toEqual([]);
  });
});

describe("agentJumpTarget", () => {
  // ws-1 holds agent a (1); ws-2 holds b (2), c (3) — flat order across groups.
  const model = buildHomeModel(
    [
      ws("ws-1", "Alpha", [tab("tab-1", leaf("a"), "a")]),
      ws("ws-2", "Beta", [
        tab("tab-2", leaf("b"), "b"),
        tab("tab-3", leaf("c"), "c"),
      ]),
    ],
    { a: agent(true), b: agent(true), c: agent(true) },
    {},
    {},
  );

  it("resolves a 1-based digit to the agent at that flat position", () => {
    expect(agentJumpTarget(model, 1)).toEqual({ wsId: "ws-1", tabId: "tab-1", leafKey: "a" });
    expect(agentJumpTarget(model, 2)).toEqual({ wsId: "ws-2", tabId: "tab-2", leafKey: "b" });
    expect(agentJumpTarget(model, 3)).toEqual({ wsId: "ws-2", tabId: "tab-3", leafKey: "c" });
  });

  it("returns null for an out-of-range digit or an empty model", () => {
    expect(agentJumpTarget(model, 4)).toBeNull();
    expect(agentJumpTarget(model, 0)).toBeNull(); // 0 = tenth; only three agents
    expect(agentJumpTarget([], 1)).toBeNull();
  });

  it("maps 0 to the tenth agent", () => {
    const ten = buildHomeModel(
      [ws("ws", "W", Array.from({ length: 10 }, (_, i) => tab(`t${i}`, leaf(`l${i}`), `l${i}`)))],
      Object.fromEntries(Array.from({ length: 10 }, (_, i) => [`l${i}`, agent(true)])),
      {},
      {},
    );
    expect(agentJumpTarget(ten, 0)).toEqual({ wsId: "ws", tabId: "t9", leafKey: "l9" });
  });
});

describe("per-agent num assignment", () => {
  const nums = (model: ReturnType<typeof buildHomeModel>) =>
    model.flatMap((w) => w.tabs.flatMap((t) => t.rows.map((r) => r.num)));

  it("numbers agents 1–9 then 0 by flat order; undefined past ten", () => {
    const model = buildHomeModel(
      [ws("ws", "W", Array.from({ length: 12 }, (_, i) => tab(`t${i}`, leaf(`l${i}`), `l${i}`)))],
      Object.fromEntries(Array.from({ length: 12 }, (_, i) => [`l${i}`, agent(true)])),
      {},
      {},
    );
    expect(nums(model)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 0, undefined, undefined]);
  });

  it("numbers are unchanged when attention states flip (R4)", () => {
    const mk = (att: Record<string, string>) =>
      buildHomeModel(
        [ws("ws", "W", [tab("t1", leaf("a"), "a"), tab("t2", leaf("b"), "b")])],
        { a: agent(true), b: agent(true) },
        {},
        att,
      );
    expect(nums(mk({}))).toEqual([1, 2]);
    expect(nums(mk({ a: "raised", b: "raised" }))).toEqual([1, 2]);
  });
});

describe("reason badge (R4/R6)", () => {
  const mk = (
    att: Record<string, string>,
    reason: Record<string, AttentionReason | null>,
  ) =>
    buildHomeModel(
      [ws("ws", "W", [tab("t1", leaf("a"), "a"), tab("t2", leaf("b"), "b")])],
      { a: agent(true), b: agent(true) },
      {},
      att,
      reason,
    );
  const rows = (model: ReturnType<typeof buildHomeModel>) =>
    model.flatMap((w) => w.tabs.flatMap((t) => t.rows));

  it("sets reason only on raised rows", () => {
    const [a, b] = rows(mk({ a: "raised", b: "acknowledged" }, { a: "question", b: "permission" }));
    expect(a.reason).toBe("question");
    expect(b.reason).toBeNull(); // acknowledged → no badge (R6)
  });

  it("leaves reason null on a raised row with no known reason", () => {
    const [a] = rows(mk({ a: "raised" }, {}));
    expect(a.reason).toBeNull();
  });

  it("ignores a stale reason on a non-raised row", () => {
    // reasonByLeaf still holds an old value for an idle pane → still no badge.
    const [, b] = rows(mk({ a: "raised", b: "idle" }, { a: "finished", b: "question" }));
    expect(b.reason).toBeNull();
  });

  it("keeps num stable when only the reason changes (R5)", () => {
    const before = rows(mk({ a: "raised", b: "raised" }, { a: "question", b: "finished" }));
    const after = rows(mk({ a: "raised", b: "raised" }, { a: "permission", b: "finished" }));
    expect(before.map((r) => r.num)).toEqual([1, 2]);
    expect(after.map((r) => r.num)).toEqual([1, 2]);
    expect(after[0].reason).toBe("permission"); // reason updated, num unchanged
  });
});

describe("firstRaised", () => {
  const mk = (att: Record<string, string>) =>
    buildHomeModel(
      [
        ws("ws-1", "Alpha", [tab("t1", leaf("a"), "a")]),
        ws("ws-2", "Beta", [tab("t2", leaf("b"), "b"), tab("t3", leaf("c"), "c")]),
      ],
      { a: agent(true), b: agent(true), c: agent(true) },
      {},
      att,
    );

  it("returns the first needs-attention row in flat order", () => {
    expect(firstRaised(mk({ b: "raised", c: "raised" }))).toEqual({
      wsId: "ws-2",
      tabId: "t2",
      leafKey: "b",
    });
  });

  it("returns null when no agent needs attention (acknowledged is not raised)", () => {
    expect(firstRaised(mk({}))).toBeNull();
    expect(firstRaised(mk({ b: "acknowledged" }))).toBeNull();
  });
});

describe("effectiveAttention", () => {
  const act = (lastOutputAgoMs: number | null): PaneActivity => ({
    isAgent: true,
    workingForMs: null,
    lastOutputAgoMs,
    liveTaskCount: 0,
  });

  it("keeps a raise with new output since you last engaged", () => {
    // engaged at 1000; last output 100ms ago at now=5000 → output at 4900 > 1000.
    const eff = effectiveAttention({ a: "raised" }, { a: act(100) }, { a: 1000 }, 5000);
    expect(eff.a).toBe("raised");
  });

  it("downgrades a stale raise (no new output since you engaged) to idle", () => {
    // engaged at 4000; last output 3000ms ago at now=5000 → output at 2000 ≤ 4000.
    const eff = effectiveAttention({ a: "raised" }, { a: act(3000) }, { a: 4000 }, 5000);
    expect(eff.a).toBe("idle");
  });

  it("passes acknowledged and idle through unchanged", () => {
    const eff = effectiveAttention(
      { a: "acknowledged", b: "idle" },
      { a: act(0), b: act(0) },
      {},
      5000,
    );
    expect(eff).toEqual({ a: "acknowledged", b: "idle" });
  });

  it("downgrades a raise with no activity record to idle", () => {
    expect(effectiveAttention({ a: "raised" }, {}, {}, 5000).a).toBe("idle");
  });

  it("keeps a raise on an agent you've never engaged", () => {
    // never engaged → engagedAt defaults to 0; any real output (now - ago > 0) counts.
    expect(effectiveAttention({ a: "raised" }, { a: act(500) }, {}, 5000).a).toBe("raised");
  });
});

describe("formatDuration", () => {
  it("formats seconds, minutes, and hours", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(999)).toBe("0s");
    expect(formatDuration(1000)).toBe("1s");
    expect(formatDuration(45_000)).toBe("45s");
    expect(formatDuration(59_000)).toBe("59s");
    expect(formatDuration(60_000)).toBe("1m"); // boundary at 60s
    expect(formatDuration(3_599_000)).toBe("59m");
    expect(formatDuration(3_600_000)).toBe("1h"); // boundary at 3600s
    expect(formatDuration(3_660_000)).toBe("1h 1m");
    expect(formatDuration(7_500_000)).toBe("2h 5m");
  });

  it("clamps negative input to 0s", () => {
    expect(formatDuration(-5000)).toBe("0s");
  });
});

describe("running state (additive background-task upgrade)", () => {
  it("upgrades only an idle base to running; waiting/working survive (decision table)", () => {
    // a–f cover every row of the plan's decision table. liveTaskCount here is the
    // already-effective (debounced) count buildHomeModel reads.
    const workspaces = [
      ws("ws-1", "Alpha", [
        tab(
          "tab-1",
          split(
            "s1",
            leaf("a"),
            split(
              "s2",
              leaf("b"),
              split("s3", leaf("c"), split("s4", leaf("d"), split("s5", leaf("e"), leaf("f")))),
            ),
          ),
          "a",
        ),
      ]),
    ];
    const model = buildHomeModel(
      workspaces,
      {
        a: agent(true, 4000, 2), // raised + tasks → waiting (attention wins, not running)
        b: agent(true, null, 0), // acknowledged + no tasks → idle
        c: agent(true, null, 3), // acknowledged + tasks → running
        d: agent(true, 5000, 2), // idle-attn + output stretch + tasks → working (output wins)
        e: agent(true, null, 0), // idle-attn + no stretch + no tasks → idle
        f: agent(true, null, 1), // idle-attn + no stretch + tasks → running
      },
      {},
      { a: "raised", b: "acknowledged", c: "acknowledged" },
    );
    const rows = model[0].tabs[0].rows;
    expect(rows.map((r) => [r.leafKey, r.status])).toEqual([
      ["a", "waiting"],
      ["b", "idle"],
      ["c", "running"],
      ["d", "working"],
      ["e", "idle"],
      ["f", "running"],
    ]);
  });

  it("never shows running when the base is working or waiting, for any count", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [tab("tab-1", split("s1", leaf("w"), leaf("r")), "w")]),
    ];
    const model = buildHomeModel(
      workspaces,
      { w: agent(true, 8000, 9), r: agent(true, 1000, 9) },
      {},
      { r: "raised" },
    );
    const byKey = Object.fromEntries(model[0].tabs[0].rows.map((r) => [r.leafKey, r.status]));
    expect(byKey.w).toBe("working"); // working base + tasks → still working (AE3)
    expect(byKey.r).toBe("waiting"); // raised base + tasks → still waiting (AE4)
  });

  it("carries liveTaskCount onto the row; running shows the count, idle is 0", () => {
    const workspaces = [
      ws("ws-1", "Alpha", [tab("tab-1", split("s1", leaf("run"), leaf("idle")), "run")]),
    ];
    const model = buildHomeModel(
      workspaces,
      { run: agent(true, null, 3), idle: agent(true, null, 0) },
      {},
      {},
    );
    const rows = Object.fromEntries(model[0].tabs[0].rows.map((r) => [r.leafKey, r]));
    expect(rows.run.status).toBe("running");
    expect(rows.run.liveTaskCount).toBe(3);
    expect(rows.idle.status).toBe("idle");
    expect(rows.idle.liveTaskCount).toBe(0);
  });

  it("running·0 is unreachable: a 0 effective count stays idle", () => {
    const workspaces = [ws("ws-1", "Alpha", [tab("tab-1", leaf("x"), "x")])];
    const model = buildHomeModel(workspaces, { x: agent(true, null, 0) }, {}, {});
    expect(model[0].tabs[0].rows[0].status).toBe("idle");
  });
});

describe("effectiveTaskCount (rise-debounce)", () => {
  const WINDOW = 3000;

  it("suppresses a count still inside the debounce window", () => {
    // rose at 1000, now 3500 → 2500 < 3000 → not yet surfaced (AE7).
    expect(effectiveTaskCount(2, 1000, 3500, WINDOW)).toBe(0);
  });

  it("surfaces the raw count once the window has elapsed", () => {
    expect(effectiveTaskCount(2, 1000, 4000, WINDOW)).toBe(2);
    expect(effectiveTaskCount(5, 1000, 9000, WINDOW)).toBe(5);
  });

  it("returns 0 for a 0 raw count regardless of riseAt (immediate fall, R7/AE6)", () => {
    expect(effectiveTaskCount(0, 1000, 9999, WINDOW)).toBe(0);
    expect(effectiveTaskCount(0, null, 9999, WINDOW)).toBe(0);
  });

  it("returns 0 when no rise has been recorded yet (riseAt null)", () => {
    expect(effectiveTaskCount(3, null, 9999, WINDOW)).toBe(0);
  });

  it("boundary: exactly windowMs elapsed surfaces (inclusive)", () => {
    expect(effectiveTaskCount(1, 1000, 4000, WINDOW)).toBe(1); // 3000 == window
    expect(effectiveTaskCount(1, 1000, 3999, WINDOW)).toBe(0); // 2999 < window
  });
});

describe("formatTaskCount", () => {
  it("pluralizes the task count (singular only at 1)", () => {
    expect(formatTaskCount(1)).toBe("1 task");
    expect(formatTaskCount(2)).toBe("2 tasks");
    expect(formatTaskCount(0)).toBe("0 tasks");
    expect(formatTaskCount(10)).toBe("10 tasks");
  });
});

describe("usageLimitLabel", () => {
  it("maps the known /usage limit kinds to their wording", () => {
    expect(usageLimitLabel({ kind: "session", scopeLabel: null })).toBe("Session");
    expect(usageLimitLabel({ kind: "weekly_all", scopeLabel: null })).toBe("Weekly · all models");
    expect(usageLimitLabel({ kind: "overage", scopeLabel: null })).toBe("Usage credits");
  });

  it("folds the model name into a per-model (weekly_scoped) limit", () => {
    expect(usageLimitLabel({ kind: "weekly_scoped", scopeLabel: "Sonnet" })).toBe("Weekly · Sonnet");
    // scoped with no model name still reads sensibly
    expect(usageLimitLabel({ kind: "weekly_scoped", scopeLabel: null })).toBe("Weekly · scoped");
  });

  it("degrades an unknown kind to a humanized label (so a new type still renders)", () => {
    expect(usageLimitLabel({ kind: "some_new_kind", scopeLabel: null })).toBe("Some new kind");
    expect(usageLimitLabel({ kind: null, scopeLabel: null })).toBe("Usage");
    // an unknown kind that carries a scope prefers the model name
    expect(usageLimitLabel({ kind: "future_thing", scopeLabel: "Opus" })).toBe("Weekly · Opus");
  });
});

describe("formatResetTime", () => {
  // America/Cancun is UTC-5 year-round (no DST), so these are deterministic.
  const tz = "America/Cancun";
  // 2026-06-30T12:00:00Z → 07:00 local, June 30.
  const now = Date.parse("2026-06-30T12:00:00Z");

  it("shows time only for a reset later the same local day (matches /usage)", () => {
    // 12:50Z → 07:50 local, same day.
    expect(formatResetTime("2026-06-30T12:50:00Z", now, tz)).toBe(
      "Resets 7:50am (America/Cancun)",
    );
  });

  it("drops :00 minutes ('8am', not '8:00am')", () => {
    // 13:00Z → 08:00 local, same day.
    expect(formatResetTime("2026-06-30T13:00:00Z", now, tz)).toBe(
      "Resets 8am (America/Cancun)",
    );
  });

  it("prefixes the date when the reset is on another local day", () => {
    // 2026-07-03T13:00:00Z → 08:00 local, July 3.
    expect(formatResetTime("2026-07-03T13:00:00Z", now, tz)).toBe(
      "Resets Jul 3, 8am (America/Cancun)",
    );
  });

  it("returns null for a null or unparseable timestamp", () => {
    expect(formatResetTime(null, now, tz)).toBeNull();
    expect(formatResetTime("not a date", now, tz)).toBeNull();
  });
});
