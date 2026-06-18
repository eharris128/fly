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
    openMenu: mk("openMenu"),
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
    // x and X are distinct entries — close pane vs close tab (R3 anti-drift).
    const xEntries = BINDINGS.filter((b) => b.keys.includes("x"));
    expect(xEntries.map((b) => b.action).sort()).toEqual([
      "closePane",
      "closeTab",
    ]);
    expect(BINDINGS.find((b) => b.action === "closeTab")?.upper).toBe(true);
    expect(BINDINGS.find((b) => b.action === "closePane")?.upper).toBeUndefined();
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

  it("leader ? opens the hotkey menu (shifted key still reaches dispatch)", () => {
    const a = spyActions();
    const km = new Keymap("ctrl+a", a);
    km.handle(ev("a", { ctrl: true }));
    expect(km.handle(ev("?", { shift: true }))).toBe(true);
    expect(a.calls).toEqual(["openMenu"]);
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
