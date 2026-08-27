// One-shot icon toolchain (not wired into any build script): generates the
// 512x512 RGBA `icon-source.png` beside this file, from which the app icon
// was derived once. No external deps — raw PNG via zlib. Design: dark
// terminal backdrop with an accent "fly" paper-plane triangle. Re-run only to
// regenerate the icon.
import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";

const S = 512;
const buf = Buffer.alloc(S * S * 4);

function px(x, y, r, g, b, a = 255) {
  const i = (y * S + x) * 4;
  buf[i] = r; buf[i + 1] = g; buf[i + 2] = b; buf[i + 3] = a;
}

// Rounded-rect test for the backdrop.
function inRoundRect(x, y, m, rad) {
  const lo = m, hi = S - m;
  if (x < lo || x > hi || y < lo || y > hi) return false;
  const cx = Math.min(Math.max(x, lo + rad), hi - rad);
  const cy = Math.min(Math.max(y, lo + rad), hi - rad);
  const dx = x - cx, dy = y - cy;
  return dx * dx + dy * dy <= rad * rad;
}

// Edge function for triangle fill.
function edge(ax, ay, bx, by, px_, py_) {
  return (px_ - ax) * (by - ay) - (py_ - ay) * (bx - ax);
}
// Paper-plane triangle vertices.
const A = [380, 256], B = [150, 148], C = [150, 364];
function inTri(x, y) {
  const d1 = edge(A[0], A[1], B[0], B[1], x, y);
  const d2 = edge(B[0], B[1], C[0], C[1], x, y);
  const d3 = edge(C[0], C[1], A[0], A[1], x, y);
  const hasNeg = d1 < 0 || d2 < 0 || d3 < 0;
  const hasPos = d1 > 0 || d2 > 0 || d3 > 0;
  return !(hasNeg && hasPos);
}
// Fold line of the plane (a darker seam from tip to back middle).
function nearFold(x, y) {
  const mx = 150, my = 256;
  // distance from point to segment A->mid
  const vx = mx - A[0], vy = my - A[1];
  const wx = x - A[0], wy = y - A[1];
  const t = Math.max(0, Math.min(1, (wx * vx + wy * vy) / (vx * vx + vy * vy)));
  const px_ = A[0] + t * vx, py_ = A[1] + t * vy;
  const dx = x - px_, dy = y - py_;
  return dx * dx + dy * dy < 9;
}

for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    if (inRoundRect(x, y, 24, 96)) {
      // vertical gradient backdrop
      const t = y / S;
      const r = Math.round(11 + t * 8);
      const g = Math.round(16 + t * 10);
      const b = Math.round(32 + t * 22);
      px(x, y, r, g, b, 255);
      if (inTri(x, y)) {
        if (nearFold(x, y)) px(x, y, 60, 120, 200, 255);
        else px(x, y, 77, 163, 255, 255);
      }
    } else {
      px(x, y, 0, 0, 0, 0);
    }
  }
}

// Assemble PNG.
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const td = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td) >>> 0, 0);
  return Buffer.concat([len, td, crc]);
}
function crc32(b) {
  let c = ~0;
  for (let i = 0; i < b.length; i++) {
    c ^= b[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c;
}
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0); ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
// raw scanlines with filter byte 0
const raw = Buffer.alloc(S * (S * 4 + 1));
for (let y = 0; y < S; y++) {
  raw[y * (S * 4 + 1)] = 0;
  buf.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
}
const png = Buffer.concat([
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
  chunk("IHDR", ihdr),
  chunk("IDAT", deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);
writeFileSync(new URL("./icon-source.png", import.meta.url), png);
console.log("wrote packaging/icon-source.png", png.length, "bytes");
