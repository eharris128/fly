// Sacred-geometry figures as circle/line primitives, in pixels relative to the figure's center.
// A figure is revealed in stages (see `reveal`): rings expand and lines trace in, innermost first.

export type Ring = { kind: "ring"; x: number; y: number; r: number };
export type Line = { kind: "line"; x1: number; y1: number; x2: number; y2: number };
export type Primitive = Ring | Line;

const PHI = (1 + Math.sqrt(5)) / 2;

function polygon(sides: number, radius: number, rotation: number): Line[] {
  const out: Line[] = [];
  for (let i = 0; i < sides; i++) {
    const a = rotation + (i / sides) * Math.PI * 2;
    const b = rotation + ((i + 1) / sides) * Math.PI * 2;
    out.push({
      kind: "line",
      x1: Math.cos(a) * radius,
      y1: Math.sin(a) * radius,
      x2: Math.cos(b) * radius,
      y2: Math.sin(b) * radius,
    });
  }
  return out;
}

// Hexagonal lattice points within `rings` steps of the center, spaced `d` apart.
function hexLattice(rings: number, d: number): Array<[number, number]> {
  const pts: Array<[number, number]> = [];
  for (let q = -rings; q <= rings; q++) {
    for (let s = -rings; s <= rings; s++) {
      if (Math.abs(q + s) > rings) continue;
      pts.push([d * (q + s / 2), d * ((s * Math.sqrt(3)) / 2)]);
    }
  }
  return pts;
}

const ring = (x: number, y: number, r: number): Ring => ({ kind: "ring", x, y, r });

const builders: Array<(R: number) => Primitive[]> = [
  // Flower of Life
  (R) => {
    const a = R / 3;
    return [...hexLattice(2, a).map(([x, y]) => ring(x, y, a)), ring(0, 0, R)];
  },
  // Seed of Life
  (R) => {
    const a = R / 2;
    return [...hexLattice(1, a).map(([x, y]) => ring(x, y, a)), ring(0, 0, R)];
  },
  // Metatron's Cube: 13 circles joined by every line between their centers
  (R) => {
    const d = R / 3;
    const pts = [...hexLattice(1, d), ...hexLattice(2, d).filter(([x, y]) => Math.hypot(x, y) > d * 1.9)];
    const out: Primitive[] = pts.map(([x, y]) => ring(x, y, d / 2.2));
    for (let i = 0; i < pts.length; i++) {
      for (let j = i + 1; j < pts.length; j++) {
        const [x1, y1] = pts[i]!;
        const [x2, y2] = pts[j]!;
        out.push({ kind: "line", x1, y1, x2, y2 });
      }
    }
    return [...out, ring(0, 0, R)];
  },
  // Hexagram in a circle
  (R) => [
    ring(0, 0, R),
    ring(0, 0, R / 2),
    ...polygon(3, R, -Math.PI / 2),
    ...polygon(3, R, Math.PI / 2),
  ],
  // Sri Yantra-like nested opposing triangles
  (R) => [
    ring(0, 0, R),
    ...[1, 0.75, 0.5, 0.3].flatMap((k, i) => polygon(3, R * k, i % 2 === 0 ? -Math.PI / 2 : Math.PI / 2)),
    ring(0, 0, R * 0.12),
  ],
  // Rosette of rotated polygons
  (R) => [
    ring(0, 0, R),
    ...[0, 1, 2, 3].flatMap((i) => polygon(8, R * (1 - i * 0.2), (i * Math.PI) / 8)),
  ],
];

function rotate(p: Primitive, angle: number): Primitive {
  const c = Math.cos(angle);
  const s = Math.sin(angle);
  if (p.kind === "ring") return { ...p, x: p.x * c - p.y * s, y: p.x * s + p.y * c };
  return {
    kind: "line",
    x1: p.x1 * c - p.y1 * s,
    y1: p.x1 * s + p.y1 * c,
    x2: p.x2 * c - p.y2 * s,
    y2: p.x2 * s + p.y2 * c,
  };
}

const reach = (p: Primitive) =>
  p.kind === "ring" ? Math.hypot(p.x, p.y) + p.r * 0.5 : Math.hypot((p.x1 + p.x2) / 2, (p.y1 + p.y2) / 2);

/** A random figure with a smaller, counter-rotated figure at the golden ratio layered inside. */
export function randomFigure(R: number): Primitive[] {
  const pick = () => builders[Math.floor(Math.random() * builders.length)]!;
  const base = pick()(R).map((p) => rotate(p, Math.random() * Math.PI));
  const inner = pick()(R / (PHI * PHI)).map((p) => rotate(p, Math.random() * Math.PI));
  return [...base, ...inner].sort((a, b) => reach(a) - reach(b));
}

const ease = (t: number) => 1 - (1 - t) * (1 - t);

/**
 * Brightness (0..1) at offset (px, py) from the center when the figure is `progress` (0..1) built.
 * `thickness` is the stroke half-width in pixels.
 */
export function figureIntensity(figure: Primitive[], px: number, py: number, progress: number, thickness: number) {
  const n = figure.length;
  let best = 0;
  for (let i = 0; i < n; i++) {
    const p = figure[i]!;
    const q = ease(Math.min(1, Math.max(0, (progress - (i / n) * 0.6) / 0.4)));
    if (q <= 0) continue;
    let d: number;
    if (p.kind === "ring") {
      d = Math.abs(Math.hypot(px - p.x, py - p.y) - p.r * q);
    } else {
      const dx = (p.x2 - p.x1) * q;
      const dy = (p.y2 - p.y1) * q;
      const len2 = dx * dx + dy * dy;
      const t = len2 === 0 ? 0 : Math.min(1, Math.max(0, ((px - p.x1) * dx + (py - p.y1) * dy) / len2));
      d = Math.hypot(px - (p.x1 + dx * t), py - (p.y1 + dy * t));
    }
    const v = 1 - d / thickness;
    if (v > best) best = v;
  }
  return best;
}
