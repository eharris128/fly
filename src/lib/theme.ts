// xterm paints to a canvas, so it can't inherit the design tokens through CSS:
// read the resolved values and hand them over as an ITheme. Re-run on a theme
// change (system scheme flip, or `data-theme` set on <html>).

import type { ITheme } from "@xterm/xterm";

type Resolve = (name: string) => string;

const fromDocument: Resolve = (name) =>
  getComputedStyle(document.documentElement).getPropertyValue(name).trim();

/** The ANSI palette for a light background, from the shared tokens. xterm's
 * built-in palette is tuned for dark backgrounds (its "white" and bright
 * yellow/cyan vanish on #f7f7f8), so light mode replaces it; dark mode keeps
 * xterm's own. "white" maps to a mid grey rather than paper white. */
function lightAnsi(resolve: Resolve): ITheme {
  const red = resolve("--color-danger");
  const green = resolve("--color-success");
  const yellow = resolve("--color-warning");
  const blue = resolve("--color-syntax-title");
  const magenta = resolve("--color-syntax-attr");
  const cyan = resolve("--color-syntax-type");
  return {
    black: resolve("--color-code-text"),
    red,
    green,
    yellow,
    blue,
    magenta,
    cyan,
    white: resolve("--color-text-muted"),
    brightBlack: resolve("--color-syntax-comment"),
    brightRed: red,
    brightGreen: green,
    brightYellow: yellow,
    brightBlue: blue,
    brightMagenta: magenta,
    brightCyan: cyan,
    brightWhite: resolve("--color-text"),
  };
}

export function terminalTheme(resolve: Resolve = fromDocument): ITheme {
  const dark = resolve("color-scheme").includes("dark");
  return {
    background: resolve("--color-bg"),
    foreground: resolve("--color-text"),
    cursor: resolve("--color-accent"),
    cursorAccent: resolve("--color-bg"),
    selectionBackground: resolve("--color-selection"),
    ...(dark ? {} : lightAnsi(resolve)),
  };
}

type Watch = (cb: () => void) => () => void;

/** One shared subscription for every terminal: App keeps all panes mounted, so
 * a per-pane matchMedia + MutationObserver would scale with pane count and
 * re-resolve the theme once per pane. The watch is installed with the first
 * subscriber and torn down with the last. */
export function createThemeBus(resolve: Resolve, watch: Watch) {
  const subs = new Set<(theme: ITheme) => void>();
  let stop: (() => void) | null = null;
  return {
    subscribe(cb: (theme: ITheme) => void): () => void {
      subs.add(cb);
      stop ??= watch(() => {
        const theme = terminalTheme(resolve);
        subs.forEach((s) => s(theme));
      });
      return () => {
        subs.delete(cb);
        if (subs.size === 0) {
          stop?.();
          stop = null;
        }
      };
    },
  };
}

const watchDocument: Watch = (cb) => {
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  mq.addEventListener("change", cb);
  const obs = new MutationObserver(cb);
  obs.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  return () => {
    mq.removeEventListener("change", cb);
    obs.disconnect();
  };
};

/** Calls `cb` with the fresh xterm theme whenever the effective theme may have
 * changed; returns the unsubscribe. */
export const onThemeChange = createThemeBus(fromDocument, watchDocument).subscribe;
