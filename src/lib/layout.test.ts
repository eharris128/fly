import { describe, it, expect, beforeEach } from "vitest";
import {
  newLeaf,
  leaves,
  splitLeaf,
  removeLeaf,
  computeRects,
  canSplit,
  neighbor,
  resetKeys,
  type Node,
  MIN_PANE_W,
} from "./layout";

beforeEach(() => resetKeys());

describe("split tree", () => {
  it("splitting a leaf creates two panes", () => {
    const root = newLeaf();
    const r = splitLeaf(root, root.key, "horizontal")!;
    expect(r).not.toBeNull();
    expect(leaves(r.tree)).toHaveLength(2);
    expect(r.tree.kind).toBe("split");
  });

  it("nested splits lay out as a tree", () => {
    const root = newLeaf();
    const a = splitLeaf(root, root.key, "horizontal")!;
    const b = splitLeaf(a.tree, a.added.key, "vertical")!;
    expect(leaves(b.tree)).toHaveLength(3);
  });

  it("closing a non-root pane collapses its parent to the sibling", () => {
    const root = newLeaf();
    const split = splitLeaf(root, root.key, "horizontal")!;
    const [first, second] = leaves(split.tree);
    const after = removeLeaf(split.tree, second.key)!;
    // The sibling takes the split's place — back to a single leaf.
    expect(after.kind).toBe("leaf");
    expect((after as { key: string }).key).toBe(first.key);
  });

  it("removing the only leaf yields null", () => {
    const root = newLeaf();
    expect(removeLeaf(root, root.key)).toBeNull();
  });

  it("splitting a missing key returns null", () => {
    const root = newLeaf();
    expect(splitLeaf(root, "nope", "horizontal")).toBeNull();
  });
});

describe("geometry + focus", () => {
  it("a horizontal split places panes left and right", () => {
    const root = newLeaf();
    const s = splitLeaf(root, root.key, "horizontal")!;
    const [left, right] = leaves(s.tree);
    const rects = computeRects(s.tree, { x: 0, y: 0, w: 1000, h: 500 });
    expect(rects.get(left.key)).toEqual({ x: 0, y: 0, w: 500, h: 500 });
    expect(rects.get(right.key)).toEqual({ x: 500, y: 0, w: 500, h: 500 });
  });

  it("directional focus finds the correct neighbor", () => {
    const root = newLeaf();
    const s = splitLeaf(root, root.key, "horizontal")!;
    const [left, right] = leaves(s.tree);
    const rects = computeRects(s.tree, { x: 0, y: 0, w: 1000, h: 500 });
    expect(neighbor(rects, left.key, "right")).toBe(right.key);
    expect(neighbor(rects, right.key, "left")).toBe(left.key);
    expect(neighbor(rects, left.key, "up")).toBeNull();
  });
});

describe("min-size clamp", () => {
  it("blocks splitting a pane that is too small", () => {
    const tiny = { x: 0, y: 0, w: MIN_PANE_W, h: 50 };
    expect(canSplit(tiny, "horizontal")).toBe(false); // half is below MIN_PANE_W
    const wide = { x: 0, y: 0, w: MIN_PANE_W * 4, h: 50 };
    expect(canSplit(wide, "horizontal")).toBe(true);
  });
});
