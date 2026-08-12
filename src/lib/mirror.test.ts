import { describe, expect, it } from "vitest";

import {
  paletteColor,
  renderMirrorHtml,
  type MirrorBuffer,
  type MirrorCell,
} from "./mirror";

function cell(overrides: Partial<Record<keyof MirrorCell, unknown>> = {}): MirrorCell {
  return {
    getChars: () => "x",
    getWidth: () => 1,
    getFgColor: () => -1,
    getBgColor: () => -1,
    isFgDefault: () => true,
    isBgDefault: () => true,
    isFgPalette: () => false,
    isBgPalette: () => false,
    isBold: () => 0,
    isItalic: () => 0,
    isDim: () => 0,
    isUnderline: () => 0,
    isInverse: () => 0,
    isInvisible: () => 0,
    ...(overrides as object),
  } as MirrorCell;
}

function bufferOf(lines: MirrorCell[][], viewportY = 0): MirrorBuffer {
  return {
    viewportY,
    getLine: (y: number) =>
      lines[y] ? { getCell: (x: number) => lines[y][x] } : undefined,
  };
}

describe("renderMirrorHtml", () => {
  it("renders default-style text without spans and escapes html", () => {
    const chars = ["<", "b", ">"];
    const line = chars.map((c) => cell({ getChars: () => c }));
    const html = renderMirrorHtml(bufferOf([line]), 1, 3);
    expect(html).toBe("&lt;b&gt;\n");
  });

  it("groups same-style cells into one span run", () => {
    const red = (c: string) =>
      cell({
        getChars: () => c,
        isFgDefault: () => false,
        isFgPalette: () => true,
        getFgColor: () => 1,
      });
    const line = [red("e"), red("r"), red("r"), cell({ getChars: () => "!" })];
    const html = renderMirrorHtml(bufferOf([line]), 1, 4);
    expect(html).toBe(`<span style="color:#cc0000;">err</span>!\n`);
  });

  it("skips wide-char continuation cells and pads empty cells", () => {
    const wide = cell({ getChars: () => "字", getWidth: () => 2 });
    const cont = cell({ getChars: () => "", getWidth: () => 0 });
    const empty = cell({ getChars: () => "" });
    const html = renderMirrorHtml(bufferOf([[wide, cont, empty]]), 1, 3);
    expect(html).toBe("字 \n");
  });

  it("renders the viewport window, not the scrollback top", () => {
    const mk = (c: string) => [cell({ getChars: () => c })];
    const html = renderMirrorHtml(bufferOf([mk("a"), mk("b"), mk("c")], 2), 1, 1);
    expect(html).toBe("c\n");
  });

  it("inverse swaps resolved colors with sensible defaults", () => {
    const inv = cell({ getChars: () => "I", isInverse: () => 1 });
    const html = renderMirrorHtml(bufferOf([[inv]]), 1, 1);
    expect(html).toContain("color:#0b1020");
    expect(html).toContain("background:#c9d1d9");
  });

  it("trims trailing blank lines", () => {
    const line = [cell({ getChars: () => "z" })];
    const html = renderMirrorHtml(bufferOf([line]), 5, 1);
    expect(html).toBe("z\n");
  });
});

describe("paletteColor", () => {
  it("maps the 16 base colors, the cube, and the gray ramp", () => {
    expect(paletteColor(1)).toBe("#cc0000");
    expect(paletteColor(16)).toBe("rgb(0,0,0)");
    expect(paletteColor(231)).toBe("rgb(255,255,255)");
    expect(paletteColor(232)).toBe("rgb(8,8,8)");
    expect(paletteColor(255)).toBe("rgb(238,238,238)");
  });
});
