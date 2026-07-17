import { describe, it, expect, beforeEach } from "vitest";
import {
  newLeaf,
  leaves,
  splitLeaf,
  removeLeaf,
  computeRects,
  canSplit,
  neighbor,
  cycleLeafKey,
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

describe("pane rotation (leader o/O)", () => {
  it("a two-pane split toggles back and forth", () => {
    const root = newLeaf();
    const s = splitLeaf(root, root.key, "horizontal")!;
    const [a, b] = leaves(s.tree);
    expect(cycleLeafKey(s.tree, a.key, 1)).toBe(b.key);
    expect(cycleLeafKey(s.tree, b.key, 1)).toBe(a.key); // wraps
    expect(cycleLeafKey(s.tree, a.key, -1)).toBe(b.key);
  });

  it("rotates through N panes in leaf order and wraps at both ends", () => {
    // horizontal split, then split the right pane vertically → 3 leaves.
    const root = newLeaf();
    const s1 = splitLeaf(root, root.key, "horizontal")!;
    const s2 = splitLeaf(s1.tree, s1.added.key, "vertical")!;
    const [a, b, c] = leaves(s2.tree);
    expect(cycleLeafKey(s2.tree, a.key, 1)).toBe(b.key);
    expect(cycleLeafKey(s2.tree, b.key, 1)).toBe(c.key);
    expect(cycleLeafKey(s2.tree, c.key, 1)).toBe(a.key); // forward wrap
    expect(cycleLeafKey(s2.tree, a.key, -1)).toBe(c.key); // backward wrap
    expect(cycleLeafKey(s2.tree, c.key, -1)).toBe(b.key);
  });

  it("a sole leaf has nothing to rotate to", () => {
    const root = newLeaf();
    expect(cycleLeafKey(root, root.key, 1)).toBeNull();
  });

  it("a stale from-key recovers to the first leaf", () => {
    const root = newLeaf();
    const s = splitLeaf(root, root.key, "vertical")!;
    const [first] = leaves(s.tree);
    expect(cycleLeafKey(s.tree, "gone", 1)).toBe(first.key);
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
