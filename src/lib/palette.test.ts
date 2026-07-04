import { describe, it, expect } from "vitest";
import {
  actionCommands,
  navCommands,
  fuzzyScore,
  filterCommands,
  type PaletteCommand,
} from "./palette";
import { BINDINGS, type KeymapActions } from "./keymap";

function spyActions(calls: string[]): KeymapActions {
  const mk = (n: string) => () => calls.push(n);
  return {
    newTab: mk("newTab"),
    splitHorizontal: mk("splitH"),
    splitVertical: mk("splitV"),
    closePane: mk("closePane"),
    closeTab: mk("closeTab"),
    focusLeft: mk("focusLeft"),
    focusRight: mk("focusRight"),
    focusUp: mk("focusUp"),
    focusDown: mk("focusDown"),
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
    prevWorkspace: mk("prev"),
    nextWorkspace: mk("next"),
    renameTab: mk("rename"),
    handoffQuick: mk("handoffQuick"),
    handoffGuided: mk("handoffGuided"),
    handoffRepick: mk("handoffRepick"),
  };
}

describe("actionCommands", () => {
  it("mirrors BINDINGS (R3/KTD1), minus the palette's own opener", () => {
    const cmds = actionCommands(spyActions([]), "ctrl+a");
    // One command per binding except openPalette — so it can't list itself.
    expect(cmds).toHaveLength(BINDINGS.length - 1);
    expect(cmds.some((c) => c.id === "action:openPalette")).toBe(false);
    expect(cmds.some((c) => c.id === "action:openMenu")).toBe(true);
  });

  it("lists the notification actions, so the palette can't drift from them", () => {
    const cmds = actionCommands(spyActions([]), "ctrl+a");
    for (const id of [
      "action:openNotifications",
      "action:jumpNewestUnread",
      "action:toggleMute",
    ]) {
      expect(cmds.some((c) => c.id === id)).toBe(true);
    }
    // leader U (jump) renders its upper key, distinct from leader u (cycle).
    expect(cmds.find((c) => c.id === "action:jumpNewestUnread")?.hint).toBe(
      "Ctrl-A U",
    );
  });

  it("lists the session-handoff actions with f/F hints (U2/R2)", () => {
    // Automatic pickup from BINDINGS — no palette wiring for the new chords.
    const cmds = actionCommands(spyActions([]), "ctrl+a");
    expect(cmds.find((c) => c.id === "action:handoffQuick")?.hint).toBe(
      "Ctrl-A f",
    );
    expect(cmds.find((c) => c.id === "action:handoffGuided")?.hint).toBe(
      "Ctrl-A F",
    );
  });

  it("shows each action's chord as the hint, cased to how it's typed", () => {
    const cmds = actionCommands(spyActions([]), "ctrl+a");
    expect(cmds.find((c) => c.id === "action:newTab")?.title).toBe("New tab");
    expect(cmds.find((c) => c.id === "action:newTab")?.hint).toBe("Ctrl-A c");
    // leader X (close tab) renders its upper key, not x.
    expect(cmds.find((c) => c.id === "action:closeTab")?.hint).toBe("Ctrl-A X");
  });

  it("run() invokes the wired action", () => {
    const calls: string[] = [];
    const cmds = actionCommands(spyActions(calls), "ctrl+a");
    cmds.find((c) => c.id === "action:newTab")?.run();
    cmds.find((c) => c.id === "action:cycleAttention")?.run();
    expect(calls).toEqual(["newTab", "cycle"]);
  });
});

describe("navCommands", () => {
  const ws = [
    {
      id: "ws-1",
      name: "default",
      tabs: [
        { id: "t-1", title: "src" },
        { id: "t-2", title: "docs" },
      ],
    },
    { id: "ws-2", name: "review", tabs: [{ id: "t-3", title: "api" }] },
  ];

  it("emits one command per workspace and per tab", () => {
    const cmds = navCommands(
      ws,
      () => {},
      () => {},
    );
    expect(cmds.filter((c) => c.hint === "workspace")).toHaveLength(2);
    expect(cmds.filter((c) => c.hint === "tab")).toHaveLength(3);
    expect(cmds.find((c) => c.id === "tab:ws-1:t-2")?.title).toBe(
      "default / docs",
    );
  });

  it("run() routes to the right selector", () => {
    const wsel: string[] = [];
    const tsel: string[] = [];
    const cmds = navCommands(
      ws,
      (w) => wsel.push(w),
      (w, t) => tsel.push(`${w}/${t}`),
    );
    cmds.find((c) => c.id === "ws:ws-2")?.run();
    cmds.find((c) => c.id === "tab:ws-2:t-3")?.run();
    expect(wsel).toEqual(["ws-2"]);
    expect(tsel).toEqual(["ws-2/t-3"]);
  });
});

describe("fuzzyScore", () => {
  it("matches subsequences case-insensitively and rejects non-matches", () => {
    expect(fuzzyScore("New tab", "nt")).not.toBeNull();
    expect(fuzzyScore("New tab", "TAB")).not.toBeNull();
    expect(fuzzyScore("New tab", "xyz")).toBeNull();
    expect(fuzzyScore("New tab", "")).toBe(0);
  });

  it("scores contiguous and leading matches better (lower)", () => {
    expect(fuzzyScore("ab", "ab")).toBe(0);
    // an interior gap costs more than a contiguous run
    expect(fuzzyScore("axb", "ab")!).toBeGreaterThan(fuzzyScore("ab", "ab")!);
    // a prefix match beats the same chars later in the string
    expect(fuzzyScore("ba", "a")!).toBeGreaterThan(fuzzyScore("ab", "a")!);
  });
});

describe("filterCommands", () => {
  const cmds: PaletteCommand[] = [
    { id: "a", title: "New tab", run: () => {} },
    { id: "b", title: "New workspace", run: () => {} },
    { id: "c", title: "Close pane", run: () => {} },
  ];

  it("returns all in source order for an empty/whitespace query", () => {
    expect(filterCommands(cmds, "").map((c) => c.id)).toEqual(["a", "b", "c"]);
    expect(filterCommands(cmds, "   ").map((c) => c.id)).toEqual(["a", "b", "c"]);
  });

  it("keeps only matches, ties in source order", () => {
    expect(filterCommands(cmds, "new").map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("drops non-matches", () => {
    expect(filterCommands(cmds, "zzz")).toEqual([]);
  });

  it("ranks a closer match ahead regardless of source order", () => {
    expect(filterCommands(cmds, "pane")[0].id).toBe("c");
  });
});
