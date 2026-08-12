// Pure snapshot renderer for unfocused-pane mirrors (tmux-substrate plan U5,
// KTD2 — revised: the mirror's source is the pane's own hidden xterm buffer,
// not tmux capture-pane, so it is substrate-agnostic and never stale).
//
// A visible-but-unfocused pane costs ~20 ms of WebKitGTK main-thread work per
// coalesced flush through xterm's live renderer (the 2026-08-11 engine-floor
// note); its display:none'd xterm still *parses* every byte (buffer stays
// current — IntersectionObserver pauses only rendering), and this module
// turns that buffer into styled DOM at snapshot cadence instead. S2 bounded
// the worst case (every word a span, full innerHTML replace, 2 Hz × 5 panes)
// at ~14% of the thread vs 63% for live rendering.
//
// Structural typing mirrors the xterm.js buffer API (IBuffer/IBufferLine/
// IBufferCell) so tests drive it with plain fakes and the component side
// passes `term.buffer.active` straight in.

/** The slice of xterm's IBufferCell the mirror reads. */
export interface MirrorCell {
  getChars(): string;
  getWidth(): number;
  getFgColor(): number;
  getBgColor(): number;
  isFgDefault(): boolean;
  isBgDefault(): boolean;
  isFgPalette(): boolean;
  isBgPalette(): boolean;
  isBold(): number;
  isItalic(): number;
  isDim(): number;
  isUnderline(): number;
  isInverse(): number;
  isInvisible(): number;
}

/** The slice of xterm's IBufferLine the mirror reads. */
export interface MirrorLine {
  getCell(x: number): MirrorCell | undefined;
}

/** The slice of xterm's IBuffer the mirror reads. */
export interface MirrorBuffer {
  viewportY: number;
  getLine(y: number): MirrorLine | undefined;
}

/** xterm's default 16-color ANSI palette (we set no custom theme). */
const ANSI16 = [
  "#2e3436", "#cc0000", "#4e9a06", "#c4a000", "#3465a4", "#75507b",
  "#06989a", "#d3d7cf", "#555753", "#ef2929", "#8ae234", "#fce94f",
  "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
];

/** 256-palette → CSS color (16 base + 6×6×6 cube + 24 grays). */
export function paletteColor(i: number): string {
  if (i < 16) return ANSI16[i] ?? "#c9d1d9";
  if (i < 232) {
    const n = i - 16;
    const steps = [0, 95, 135, 175, 215, 255];
    const r = steps[Math.floor(n / 36) % 6];
    const g = steps[Math.floor(n / 6) % 6];
    const b = steps[n % 6];
    return `rgb(${r},${g},${b})`;
  }
  const v = 8 + (i - 232) * 10;
  return `rgb(${v},${v},${v})`;
}

/** Non-palette color values are 24-bit RGB packed as 0xRRGGBB. */
function rgbColor(v: number): string {
  return `rgb(${(v >> 16) & 0xff},${(v >> 8) & 0xff},${v & 0xff})`;
}

function escapeHtml(s: string): string {
  return s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

/** One style run's CSS, or "" for fully-default text (no span emitted). */
function cellStyle(cell: MirrorCell): string {
  let fg = "";
  let bg = "";
  if (!cell.isFgDefault()) {
    fg = cell.isFgPalette() ? paletteColor(cell.getFgColor()) : rgbColor(cell.getFgColor());
  }
  if (!cell.isBgDefault()) {
    bg = cell.isBgPalette() ? paletteColor(cell.getBgColor()) : rgbColor(cell.getBgColor());
  }
  if (cell.isInverse()) {
    const f = fg || "#c9d1d9";
    const b = bg || "#0b1020";
    fg = b;
    bg = f;
  }
  let css = "";
  if (fg) css += `color:${fg};`;
  if (bg) css += `background:${bg};`;
  if (cell.isBold()) css += "font-weight:700;";
  if (cell.isItalic()) css += "font-style:italic;";
  if (cell.isDim()) css += "opacity:.6;";
  if (cell.isUnderline()) css += "text-decoration:underline;";
  if (cell.isInvisible()) css += "visibility:hidden;";
  return css;
}

/**
 * Render the buffer's current viewport (`rows` × `cols` from `viewportY`) to
 * an HTML string: one text node or `<span style=…>` per same-style run, one
 * `\n` per line (the container is a `<pre>`). Wide chars occupy one cell with
 * width 2 followed by a width-0 continuation cell, which is skipped.
 */
export function renderMirrorHtml(
  buf: MirrorBuffer,
  rows: number,
  cols: number,
): string {
  const out: string[] = [];
  for (let y = 0; y < rows; y++) {
    const line = buf.getLine(buf.viewportY + y);
    if (!line) {
      out.push("\n");
      continue;
    }
    let runText = "";
    let runStyle = "";
    const flush = () => {
      if (!runText) return;
      const text = escapeHtml(runText);
      out.push(runStyle ? `<span style="${runStyle}">${text}</span>` : text);
      runText = "";
    };
    for (let x = 0; x < cols; x++) {
      const cell = line.getCell(x);
      if (!cell || cell.getWidth() === 0) continue; // wide-char continuation
      const style = cellStyle(cell);
      if (style !== runStyle) {
        flush();
        runStyle = style;
      }
      runText += cell.getChars() || " ";
    }
    flush();
    out.push("\n");
  }
  // Trim trailing blank lines so short buffers don't scroll the mirror.
  let html = out.join("");
  html = html.replace(/\n+$/, "\n");
  return html;
}
