<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { WebglAddon } from "@xterm/addon-webgl";
  import { Unicode11Addon } from "@xterm/addon-unicode11";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import "@xterm/xterm/css/xterm.css";
  import {
    spawnPane,
    ptyWrite,
    ptyResize,
    closePane,
    makeOutputChannel,
    type PaneId,
    type PaneExitEvent,
  } from "../ipc";

  let container: HTMLDivElement;
  let term: Terminal | undefined;
  let fit: FitAddon | undefined;
  let paneId: PaneId | null = null;
  let unlistenExit: UnlistenFn | null = null;
  let resizeObs: ResizeObserver | null = null;

  onMount(async () => {
    term = new Terminal({
      fontFamily: "ui-monospace, 'JetBrains Mono', Menlo, Consolas, monospace",
      fontSize: 13,
      cursorBlink: true,
      scrollback: 10_000,
      allowProposedApi: true, // required by the unicode11 addon
      theme: { background: "#0b1020", foreground: "#c9d1d9" },
    });

    fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = "11";

    term.open(container);

    // Prefer the WebGL renderer; fall back to the DOM renderer on failure or
    // context loss. The full eviction policy lands in U4/U6.
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch (err) {
      console.warn("WebGL renderer unavailable, using DOM renderer", err);
    }

    fit.fit();
    const cols = term.cols >= 2 ? term.cols : 80;
    const rows = term.rows >= 2 ? term.rows : 24;

    const channel = makeOutputChannel((bytes) => term?.write(bytes));
    paneId = await spawnPane(channel, { rows, cols });

    // Keystrokes (including control bytes like Ctrl-C = 0x03) pass straight to
    // the PTY. Leader-key interception arrives in U6.
    term.onData((data) => {
      if (paneId !== null) void ptyWrite(paneId, data);
    });

    // Re-fit and propagate geometry on container resize (debounced by the
    // browser's rAF coalescing of ResizeObserver).
    resizeObs = new ResizeObserver(() => {
      if (!fit || !term) return;
      fit.fit();
      if (paneId !== null) void ptyResize(paneId, term.rows, term.cols);
    });
    resizeObs.observe(container);

    unlistenExit = await listen<PaneExitEvent>("pane://exit", (ev) => {
      if (ev.payload.paneId !== paneId || !term) return;
      const s = ev.payload.state;
      const note =
        s.kind === "exited"
          ? `process exited (code ${s.code}${s.signal ? `, ${s.signal}` : ""})`
          : `process ${s.kind}`;
      term.write(`\r\n\x1b[2m[${note}]\x1b[0m\r\n`);
    });

    term.focus();
  });

  onDestroy(() => {
    resizeObs?.disconnect();
    unlistenExit?.();
    if (paneId !== null) void closePane(paneId);
    term?.dispose();
  });
</script>

<div class="terminal" bind:this={container}></div>

<style>
  .terminal {
    width: 100%;
    height: 100%;
    padding: 4px;
    background: #0b1020;
  }
</style>
