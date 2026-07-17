// Split-tree layout model (U5).
//
// A tab's layout is a binary tree: leaves are panes, internal nodes are splits
// (orientation + ratio). The tree lives frontend-side for v1's single
// in-process window; the backend stays the PaneId authority and pane ops route
// through it by id (KTD5). Operations here are pure and return new trees.

export type Orientation = "horizontal" | "vertical";

export interface Leaf {
  kind: "leaf";
  key: string;
}
export interface Split {
  kind: "split";
  key: string;
  orientation: Orientation;
  ratio: number; // fraction of space given to `first`
  first: Node;
  second: Node;
}
export type Node = Leaf | Split;

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

// Minimum usable pane size in px (≈ 20 cols × 4 rows), used to block oversplits.
export const MIN_PANE_W = 160;
export const MIN_PANE_H = 80;

let counter = 0;
/** Reset the key counter (tests only). */
export function resetKeys(): void {
  counter = 0;
}
function nextKey(prefix: string): string {
  counter += 1;
  return `${prefix}-${counter}`;
}

export function newLeaf(): Leaf {
  return { kind: "leaf", key: nextKey("leaf") };
}

/** Every node key in the tree (leaf + split), for restore collision-avoidance. */
export function collectKeys(node: Node): string[] {
  return node.kind === "leaf"
    ? [node.key]
    : [node.key, ...collectKeys(node.first), ...collectKeys(node.second)];
}

/** Advance the key counter past any restored keys so new nodes never collide. */
export function ensureKeyCounterAbove(keys: string[]): void {
  for (const k of keys) {
    const m = k.match(/-(\d+)$/);
    if (m) counter = Math.max(counter, Number.parseInt(m[1], 10));
  }
}

/** All leaves, left-to-right / top-to-bottom. */
export function leaves(node: Node): Leaf[] {
  return node.kind === "leaf"
    ? [node]
    : [...leaves(node.first), ...leaves(node.second)];
}

/**
 * The leaf `delta` steps from `fromKey` in leaf order (left-to-right /
 * top-to-bottom, wrapping), for the leader o/O focus rotation. Rotation order
 * is `leaves()` order — the same stable order splits render in — so it scales
 * to any pane count, not just a two-pane toggle. Returns `null` when there is
 * nothing to rotate to (a sole leaf); a stale `fromKey` recovers to the first
 * leaf rather than dead-ending focus.
 */
export function cycleLeafKey(
  root: Node,
  fromKey: string,
  delta: 1 | -1 = 1,
): string | null {
  const ls = leaves(root);
  if (ls.length < 2) return null;
  const i = ls.findIndex((l) => l.key === fromKey);
  if (i === -1) return ls[0].key;
  return ls[(i + delta + ls.length) % ls.length].key;
}

/** Split `targetKey` into [existing, new] along `orientation`. */
export function splitLeaf(
  root: Node,
  targetKey: string,
  orientation: Orientation,
): { tree: Node; added: Leaf } | null {
  const added = newLeaf();
  const replace = (node: Node): Node | null => {
    if (node.kind === "leaf") {
      if (node.key !== targetKey) return null;
      return {
        kind: "split",
        key: nextKey("split"),
        orientation,
        ratio: 0.5,
        first: node,
        second: added,
      };
    }
    const f = replace(node.first);
    if (f) return { ...node, first: f };
    const s = replace(node.second);
    if (s) return { ...node, second: s };
    return null;
  };
  const tree = replace(root);
  return tree ? { tree, added } : null;
}

/**
 * Remove leaf `key`; its parent split collapses so the sibling takes the
 * parent's place. Returns the new tree, or `null` if it was the only leaf.
 */
export function removeLeaf(root: Node, key: string): Node | null {
  if (root.kind === "leaf") return root.key === key ? null : root;
  const first = removeLeaf(root.first, key);
  if (first === null) return root.second; // first subtree was the leaf
  if (first !== root.first) return { ...root, first };
  const second = removeLeaf(root.second, key);
  if (second === null) return root.first; // second subtree was the leaf
  if (second !== root.second) return { ...root, second };
  return root; // not found
}

/** Assign a screen rect to every leaf, given the container rect. */
export function computeRects(
  node: Node,
  rect: Rect,
  out: Map<string, Rect> = new Map(),
): Map<string, Rect> {
  if (node.kind === "leaf") {
    out.set(node.key, rect);
    return out;
  }
  if (node.orientation === "horizontal") {
    const w1 = rect.w * node.ratio;
    computeRects(node.first, { x: rect.x, y: rect.y, w: w1, h: rect.h }, out);
    computeRects(
      node.second,
      { x: rect.x + w1, y: rect.y, w: rect.w - w1, h: rect.h },
      out,
    );
  } else {
    const h1 = rect.h * node.ratio;
    computeRects(node.first, { x: rect.x, y: rect.y, w: rect.w, h: h1 }, out);
    computeRects(
      node.second,
      { x: rect.x, y: rect.y + h1, w: rect.w, h: rect.h - h1 },
      out,
    );
  }
  return out;
}

export interface DividerRect {
  splitKey: string;
  orientation: Orientation;
  rect: Rect;
  /** The rect being divided — lets a drag compute the ratio for nested splits. */
  parent: Rect;
}

/** Divider strips (between split children) for the active tab, for drag-resize. */
export function dividers(
  node: Node,
  rect: Rect,
  out: DividerRect[] = [],
): DividerRect[] {
  if (node.kind === "leaf") return out;
  if (node.orientation === "horizontal") {
    const w1 = rect.w * node.ratio;
    out.push({
      splitKey: node.key,
      orientation: "horizontal",
      rect: { x: rect.x + w1 - 2, y: rect.y, w: 4, h: rect.h },
      parent: rect,
    });
    dividers(node.first, { x: rect.x, y: rect.y, w: w1, h: rect.h }, out);
    dividers(node.second, { x: rect.x + w1, y: rect.y, w: rect.w - w1, h: rect.h }, out);
  } else {
    const h1 = rect.h * node.ratio;
    out.push({
      splitKey: node.key,
      orientation: "vertical",
      rect: { x: rect.x, y: rect.y + h1 - 2, w: rect.w, h: 4 },
      parent: rect,
    });
    dividers(node.first, { x: rect.x, y: rect.y, w: rect.w, h: h1 }, out);
    dividers(node.second, { x: rect.x, y: rect.y + h1, w: rect.w, h: rect.h - h1 }, out);
  }
  return out;
}

/** Update a split node's ratio, returning a new tree. */
export function setRatio(root: Node, splitKey: string, ratio: number): Node {
  if (root.kind === "leaf") return root;
  if (root.key === splitKey) return { ...root, ratio };
  return {
    ...root,
    first: setRatio(root.first, splitKey, ratio),
    second: setRatio(root.second, splitKey, ratio),
  };
}

/** Whether `rect` is large enough to split into two min-size panes. */
export function canSplit(rect: Rect, orientation: Orientation): boolean {
  return orientation === "horizontal"
    ? rect.w / 2 >= MIN_PANE_W
    : rect.h / 2 >= MIN_PANE_H;
}

const overlapsY = (a: Rect, b: Rect) => a.y < b.y + b.h && b.y < a.y + a.h;
const overlapsX = (a: Rect, b: Rect) => a.x < b.x + b.w && b.x < a.x + a.w;

export type Direction = "left" | "right" | "up" | "down";

/** The nearest leaf in `dir` from `fromKey`, by center distance with edge + perpendicular-overlap gating. */
export function neighbor(
  rects: Map<string, Rect>,
  fromKey: string,
  dir: Direction,
): string | null {
  const from = rects.get(fromKey);
  if (!from) return null;
  const fcx = from.x + from.w / 2;
  const fcy = from.y + from.h / 2;
  let best: string | null = null;
  let bestDist = Infinity;
  for (const [key, r] of rects) {
    if (key === fromKey) continue;
    let inDir = false;
    if (dir === "left") inDir = r.x + r.w <= from.x + 1 && overlapsY(from, r);
    else if (dir === "right") inDir = r.x >= from.x + from.w - 1 && overlapsY(from, r);
    else if (dir === "up") inDir = r.y + r.h <= from.y + 1 && overlapsX(from, r);
    else inDir = r.y >= from.y + from.h - 1 && overlapsX(from, r);
    if (!inDir) continue;
    const d = Math.hypot(r.x + r.w / 2 - fcx, r.y + r.h / 2 - fcy);
    if (d < bestDist) {
      bestDist = d;
      best = key;
    }
  }
  return best;
}
