import { describe, expect, it, vi } from "vitest";
import { createThemeBus, terminalTheme } from "./theme";

const vars: Record<string, string> = {
  "color-scheme": "dark",
  "--color-bg": "#15151b",
  "--color-text": "#ececf1",
  "--color-text-muted": "#a9a9b8",
  "--color-accent": "#818cf8",
  "--color-selection": "#3b3f7a",
  "--color-danger": "#ef6b6b",
  "--color-success": "#6cc070",
  "--color-warning": "#f0a54a",
  "--color-code-text": "#e3e3ea",
  "--color-syntax-title": "#8ab4ff",
  "--color-syntax-attr": "#f39ac4",
  "--color-syntax-type": "#6fd3e0",
  "--color-syntax-comment": "#8b8b9c",
};
const resolve = (n: string) => vars[n] ?? "";

describe("terminalTheme", () => {
  it("maps design tokens onto the xterm theme and keeps xterm's ANSI palette in dark", () => {
    expect(terminalTheme(resolve)).toEqual({
      background: "#15151b",
      foreground: "#ececf1",
      cursor: "#818cf8",
      cursorAccent: "#15151b",
      selectionBackground: "#3b3f7a",
    });
  });

  it("replaces the ANSI palette in light so white/bright text stays readable", () => {
    const t = terminalTheme((n) => (n === "color-scheme" ? "light" : resolve(n)));
    expect(t.white).toBe("#a9a9b8");
    expect(t.brightWhite).toBe("#ececf1");
    expect(t.red).toBe("#ef6b6b");
    expect(t.black).toBe("#e3e3ea");
  });
});

describe("createThemeBus", () => {
  it("installs one watch for all subscribers and resolves the theme once per change", () => {
    let fire = () => {};
    const stop = vi.fn();
    const watch = vi.fn((cb: () => void) => {
      fire = cb;
      return stop;
    });
    const spy = vi.fn(resolve);
    const bus = createThemeBus(spy, watch);
    const a = vi.fn();
    const b = vi.fn();
    const offA = bus.subscribe(a);
    const offB = bus.subscribe(b);
    expect(watch).toHaveBeenCalledTimes(1);

    fire();
    const callsPerResolve = spy.mock.calls.length;
    expect(a).toHaveBeenCalledTimes(1);
    expect(a.mock.calls[0][0]).toBe(b.mock.calls[0][0]); // the same theme object, resolved once
    fire();
    expect(spy.mock.calls.length).toBe(callsPerResolve * 2);

    offA();
    expect(stop).not.toHaveBeenCalled();
    offB();
    expect(stop).toHaveBeenCalledTimes(1);
  });

  it("re-installs the watch for a subscriber after the last one left", () => {
    const watch = vi.fn(() => () => {});
    const bus = createThemeBus(resolve, watch);
    bus.subscribe(() => {})();
    bus.subscribe(() => {});
    expect(watch).toHaveBeenCalledTimes(2);
  });
});
