import { describe, expect, it } from "vitest";

import { prunePaneIdMaps, resolveSpawnRace } from "./pane-maps";

describe("resolveSpawnRace (U9/KTD9 destroyed guard)", () => {
  it("a spawn resolving after destroy closes the pane and never announces", () => {
    expect(resolveSpawnRace(true)).toEqual({ closeNow: true, announce: false });
  });

  it("a normal spawn adopts the pane and announces it", () => {
    expect(resolveSpawnRace(false)).toEqual({ closeNow: false, announce: true });
  });
});

describe("prunePaneIdMaps (U9/KTD9)", () => {
  it("drops closed leaves from both maps and keeps live ones", () => {
    const { paneIdByLeaf, leafByPaneId } = prunePaneIdMaps(
      new Set(["leaf-live"]),
      { "leaf-live": 3, "leaf-closed": 4 },
      { 3: "leaf-live", 4: "leaf-closed" },
    );
    expect(paneIdByLeaf).toEqual({ "leaf-live": 3 });
    expect(leafByPaneId).toEqual({ 3: "leaf-live" });
  });

  it("drops a stale reverse entry whose pane id was reused by another leaf", () => {
    // Pane id 5 was reused: forward map says leaf-b owns it now, but an old
    // reverse entry could claim leaf-a. Only the coherent pair survives.
    const { leafByPaneId } = prunePaneIdMaps(
      new Set(["leaf-a", "leaf-b"]),
      { "leaf-a": 9, "leaf-b": 5 },
      { 5: "leaf-a" },
    );
    expect(leafByPaneId).toEqual({});
  });

  it("handles tab/workspace-scale closures (whole subtree gone) and empty maps", () => {
    const { paneIdByLeaf, leafByPaneId } = prunePaneIdMaps(
      new Set(),
      { "t1-l1": 1, "t1-l2": 2 },
      { 1: "t1-l1", 2: "t1-l2" },
    );
    expect(paneIdByLeaf).toEqual({});
    expect(leafByPaneId).toEqual({});
    expect(prunePaneIdMaps(new Set(["x"]), {}, {})).toEqual({
      paneIdByLeaf: {},
      leafByPaneId: {},
    });
  });

  it("does not mutate its inputs", () => {
    const fwd = { a: 1 };
    const rev = { 1: "a" };
    prunePaneIdMaps(new Set(), fwd, rev);
    expect(fwd).toEqual({ a: 1 });
    expect(rev).toEqual({ 1: "a" });
  });
});
