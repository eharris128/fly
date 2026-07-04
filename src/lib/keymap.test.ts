import { describe, it, expect } from "vitest";
import {
  Keymap,
  parseLeader,
  formatLeader,
  BINDINGS,
  type KeymapActions,
} from "./keymap";

function ev(
  key: string,
  mods: { ctrl?: boolean; meta?: boolean; alt?: boolean; shift?: boolean } = {},
  type = "keydown",
): KeyboardEvent {
  return {
    type,
    key,
    ctrlKey: !!mods.ctrl,
    metaKey: !!mods.meta,
    altKey: !!mods.alt,
    shiftKey: !!mods.shift,
  } as KeyboardEvent;
}

function spyActions(): KeymapActions & { calls: string[] } {
  const calls: string[] = [];
  const mk = (name: string) => () => calls.push(name);
  return {
    calls,
    newTab: mk("newTab"),
    splitHorizontal: mk("splitH"),
    splitVertical: mk("splitV"),
    closePane: mk("close"),
    closeTab: mk("closeTab"),
    focusLeft: mk("left"),
    focusRight: mk("right"),
    focusUp: mk("up"),
    focusDown: mk("down"),
    cycleAttention: mk("cycle"),
    jumpNewestUnread: mk("jumpUnread"),
    openNotifications: mk("openNotifications"),
    toggleMute: mk("toggleMute"),
    openMenu: mk("openMenu"),
    openPalette: mk("openPalette"),
    openSettings: mk("openSettings"),
    toggleSidebar: mk("toggleSidebar"),
    toggleHome: mk("toggleHome"),
    newWorkspace: mk("newWorkspace"),
    closeWorkspace: mk("closeWorkspace"),
    prevWorkspace: mk("prevWorkspace"),
    nextWorkspace: mk("nextWorkspace"),
    renameTab: mk("renameTab"),
    handoffQuick: mk("handoffQuick"),
    handoffGuided: mk("handoffGuided"),
    handoffRepick: mk("handoffRepick"),
  };
}

describe("formatLeader", () => {
  it("renders leader specs for the hotkey menu", () => {
    expect(formatLeader("ctrl+a")).toBe("Ctrl-A");
    expect(formatLeader("super+space")).toBe("Super-Space");
    expect(formatLeader("alt+shift+x")).toBe("Alt-Shift-X");
  });
});

describe("BINDINGS", () => {
  it("is the single source of truth and carries the new chords", () => {
    const actions = BINDINGS.map((b) => b.action);
    expect(actions).toContain("openMenu");
    expect(actions).toContain("openPalette");
    expect(BINDINGS.find((b) => b.action === "openPalette")?.keys).toEqual(["p"]);
    // x and X are distinct entries — close pane vs close tab (R3 anti-drift).
    const xEntries = BINDINGS.filter((b) => b.keys.includes("x"));
    expect(xEntries.map((b) => b.action).sort()).toEqual([
      "closePane",
      "closeTab",
    ]);
    expect(BINDINGS.find((b) => b.action === "closeTab")?.upper).toBe(true);
    expect(BINDINGS.find((b) => b.action === "closePane")?.upper).toBeUndefined();
    // w and W are distinct entries — new workspace vs close workspace.
    const wEntries = BINDINGS.filter((b) => b.keys.includes("w"));
    expect(wEntries.map((b) => b.action).sort()).toEqual([
      "closeWorkspace",
      "newWorkspace",
    ]);
    expect(BINDINGS.find((b) => b.action === "closeWorkspace")?.upper).toBe(true);
    expect(BINDINGS.find((b) => b.action === "newWorkspace")?.upper).toBeUndefined();
  });

  it("carries the notification chords with u/U disambiguated", () => {
    const actions = BINDINGS.map((b) => b.action);
    expect(actions).toContain("openNotifications");
    expect(actions).toContain("toggleMute");
    // u and U are distinct entries — cycle attention vs jump-to-unread.
    const uEntries = BINDINGS.filter((b) => b.keys.includes("u"));
    expect(uEntries.map((b) => b.action).sort()).toEqual([
      "cycleAttention",
      "jumpNewestUnread",
    ]);
    expect(BINDINGS.find((b) => b.action === "jumpNewestUnread")?.upper).toBe(true);
    expect(BINDINGS.find((b) => b.action === "cycleAttention")?.upper).toBeUndefined();
  });

  it("carries the session-handoff chords with f/F disambiguated (U2/R1)", () => {
    // f and F are distinct entries — quick vs guided handoff, like x / X.
    const fEntries = BINDINGS.filter((b) => b.keys.includes("f"));
    expect(fEntries.map((b) => b.action).sort()).toEqual([
      "handoffGuided",
      "handoffQuick",
    ]);
    expect(BINDINGS.find((b) => b.action === "handoffGuided")?.upper).toBe(true);
    expect(
      BINDINGS.find((b) => b.action === "handoffQuick")?.upper,
    ).toBeUndefined();
  });

  it("carries the re-pick chord beside the handoffs (fix-attribution U8/R14)", () => {
    // leader g = reset attribution + forced pick-list, in the shared table so
    // dispatch, the cheat-sheet, and the palette can't drift.
    const g = BINDINGS.find((b) => b.keys.includes("g"));
    expect(g?.action).toBe("handoffRepick");
    expect(g?.upper).toBeUndefined();
  });

  it("no two bindings collide on the same key + case", () => {
    // A duplicate (key, upper) pair would make dispatch() order-dependent —
    // one of the two actions silently unreachable. Global guard, so any new
    // chord (the handoff f/F rows included, U2/R1) is proven unique.
    const seen = new Set<string>();
    for (const b of BINDINGS) {
      for (const k of b.keys) {
        const id = `${b.upper ? "upper" : "lower"}:${k}`;
        expect(seen.has(id), `duplicate chord ${id}`).toBe(false);
        seen.add(id);
      }
    }
  });

  it("carries the dashboard toggle on leader d", () => {
    const home = BINDINGS.filter((b) => b.action === "toggleHome");
    expect(home).toHaveLength(1);
    expect(home[0].keys).toEqual(["d"]);
    expect(home[0].upper).toBeUndefined();
    expect(home[0].label).toBe("Dashboard (home)");
  });
});

describe("parseLeader", () => {
  it("matches the exact leader chord only", () => {
    const m = parseLeader("ctrl+a");
    expect(m(ev("a", { ctrl: true }))).toBe(true);
    expect(m(ev("a"))).toBe(false); // no ctrl
    expect(m(ev("b", { ctrl: true }))).toBe(false);
    expect(m(ev("a", { ctrl: true, shift: true }))).toBe(false); // extra modifier
  });
});

describe("Keymap", () => {
  it("leader then a command runs the action and consumes both", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    expect(km.handle(ev("a", { ctrl: true }))).toBe(true); // leader consumed
    expect(a.calls).toEqual([]);
    expect(km.handle(ev("|", { shift: true }))).toBe(true); // command consumed
    expect(a.calls).toEqual(["splitH"]);
  });

  it("leader d toggles the dashboard home view", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    expect(km.handle(ev("a", { ctrl: true }))).toBe(true);
    expect(km.handle(ev("d"))).toBe(true); // command consumed, never reaches PTY
    expect(a.calls).toEqual(["toggleHome"]);
  });

  it("passes ordinary input through to the PTY", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    expect(km.handle(ev("c", { ctrl: true }))).toBe(false); // Ctrl-C → shell
    expect(km.handle(ev("w", { ctrl: true }))).toBe(false); // Ctrl-W → program
    expect(km.handle(ev("j"))).toBe(false); // vim nav
    expect(a.calls).toEqual([]);
  });

  it("an unbound leader chord is a consumed no-op (never leaks)", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("z"))).toBe(true); // consumed
    expect(a.calls).toEqual([]);
    // Subsequent ordinary input still passes through.
    expect(km.handle(ev("z"))).toBe(false);
  });

  it("maps focus + tab + close chords", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    const chord = (k: string, mods = {}) => {
      km.handle(ev("a", { ctrl: true }));
      km.handle(ev(k, mods));
    };
    chord("h");
    chord("l");
    chord("c");
    chord("x");
    chord("u");
    expect(a.calls).toEqual(["left", "right", "newTab", "close", "cycle"]);
  });

  it("every split chord form fires after the BINDINGS refactor", () => {
    // The refactor (U1) is the one change that could silently regress a
    // shifted-symbol chord; the old suite only proved "|". Cover all forms.
    const cases: Array<[string, { shift?: boolean }, string]> = [
      ["|", { shift: true }, "splitH"], // Shift+\
      ["\\", {}, "splitH"], // unshifted alias
      ["-", {}, "splitV"],
      ["_", { shift: true }, "splitV"], // Shift+-
    ];
    for (const [key, mods, expected] of cases) {
      const a = spyActions();
      const km = new Keymap("ctrl+a", a);
      km.handle(ev("a", { ctrl: true }));
      expect(km.handle(ev(key, mods))).toBe(true);
      expect(a.calls).toEqual([expected]);
    }
  });

  it("distinguishes leader x (close pane) from leader X (close tab)", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true }));
    km.handle(ev("x")); // literal lowercase → close pane
    km.handle(ev("a", { ctrl: true }));
    km.handle(ev("X", { shift: true })); // literal uppercase → close tab
    expect(a.calls).toEqual(["close", "closeTab"]);
  });

  it("maps the workspace + sidebar chords", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    const chord = (k: string, mods = {}) => {
      km.handle(ev("a", { ctrl: true }));
      expect(km.handle(ev(k, mods))).toBe(true);
    };
    chord("b");
    chord("w"); // lowercase → new workspace (no regression)
    chord("W", { shift: true }); // uppercase → close workspace
    chord("[");
    chord("]");
    chord("r");
    expect(a.calls).toEqual([
      "toggleSidebar",
      "newWorkspace",
      "closeWorkspace",
      "prevWorkspace",
      "nextWorkspace",
      "renameTab",
    ]);
  });

  it("leader ? opens the hotkey menu (shifted key still reaches dispatch)", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("?", { shift: true }))).toBe(true);
    expect(a.calls).toEqual(["openMenu"]);
  });

  it("leader p opens the command palette", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("p"))).toBe(true);
    expect(a.calls).toEqual(["openPalette"]);
  });

  it("maps the notification chords; leader u/U stay distinct", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    const chord = (k: string, mods = {}) => {
      km.handle(ev("a", { ctrl: true }));
      expect(km.handle(ev(k, mods))).toBe(true);
    };
    chord("n"); // notifications panel
    chord("m"); // toggle mute
    chord("u"); // lowercase → cycle attention (no regression)
    chord("U", { shift: true }); // uppercase → jump to newest unread
    expect(a.calls).toEqual([
      "openNotifications",
      "toggleMute",
      "cycle",
      "jumpUnread",
    ]);
  });

  it("distinguishes leader f (quick handoff) from leader F (guided)", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("f"))).toBe(true); // literal lowercase → quick
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("F", { shift: true }))).toBe(true); // uppercase → guided
    expect(a.calls).toEqual(["handoffQuick", "handoffGuided"]);
  });

  it("a bare modifier keydown does not consume the pending leader", () => {
    // The browser delivers a "Shift" keydown of its own before the shifted
    // key. Without skipping it, the leader would be cleared and chords like
    // ? / X / | / _ would never fire.
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true })); // leader
    expect(km.handle(ev("Shift", { shift: true }))).toBe(true); // swallowed
    expect(a.calls).toEqual([]); // still pending — nothing dispatched yet
    expect(km.handle(ev("?", { shift: true }))).toBe(true); // real key
    expect(a.calls).toEqual(["openMenu"]);
  });

  it("a configurable leader works (super+space)", () => {
    const a = spyActions();
    const km = new Keymap("super+space", a);
    expect(km.handle(ev(" ", { meta: true }))).toBe(true);
    km.handle(ev("-"));
    expect(a.calls).toEqual(["splitV"]);
  });
});

describe("Keymap digit chords (U1)", () => {
  function withDigits() {
    const a = spyActions();
    const digits: number[] = [];
    const km = new Keymap("ctrl+a", a, (n) => digits.push(n));
    return { a, digits, km };
  }

  it("leader then 1–9 dispatches the digit (1-based) and consumes the chord", () => {
    const { a, digits, km } = withDigits();
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("3"))).toBe(true);
    expect(digits).toEqual([3]);
    km.handle(ev("a", { ctrl: true }));
    km.handle(ev("1"));
    expect(digits).toEqual([3, 1]); // 1-based, not 0-based
    km.handle(ev("a", { ctrl: true }));
    km.handle(ev("9"));
    // 9 is passed through verbatim; the App resolver decides out-of-range is a
    // no-op (it has no tab 9), so that lives at the App layer, not here.
    expect(digits).toEqual([3, 1, 9]);
    expect(a.calls).toEqual([]); // never fires a workspace or other action
  });

  it("leader then 0 is a consumed no-op (no digit dispatched, never leaks)", () => {
    const { a, digits, km } = withDigits();
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("0"))).toBe(true); // consumed
    expect(digits).toEqual([]);
    expect(a.calls).toEqual([]);
  });

  it("a bare digit with no pending leader passes through to the PTY", () => {
    const { digits, km } = withDigits();
    expect(km.handle(ev("5"))).toBe(false); // not consumed → reaches the shell
    expect(digits).toEqual([]);
  });

  it("a shifted digit (Shift+1 → '!') does not select a tab", () => {
    const { a, digits, km } = withDigits();
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("!", { shift: true }))).toBe(true); // consumed no-op
    expect(digits).toEqual([]);
    expect(a.calls).toEqual([]);
  });

  it("the leader→digit chord survives a focus change between the keypresses", () => {
    // One shared Keymap instance serves both the xterm and window key paths, so
    // a leader pressed in one and the digit arriving via the other completes
    // the chord exactly once.
    const { digits, km } = withDigits();
    km.handle(ev("a", { ctrl: true })); // leader, e.g. from a focused pane
    km.handle(ev("2")); // digit, e.g. via the window listener after a click
    expect(digits).toEqual([2]);
  });
});
