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
    setPaneFocus,
    setWindowForeground,
    type PaneId,
    type PaneExitEvent,
    type AttentionEvent,
    type AttentionState,
    type AttentionReason,
  } from "../ipc";

  let container: HTMLDivElement;
  let term: Terminal | undefined;
  let fit: FitAddon | undefined;
  let paneId: PaneId | null = null;
  let unlisteners: UnlistenFn[] = [];
  let resizeObs: ResizeObserver | null = null;

  // Reactive attention state drives the visual indicator.
  let attention = $state<AttentionState>("idle");
  let reason = $state<AttentionReason | null>(null);

  const REASON_LABEL: Record<AttentionReason, string> = {
    question: "waiting for you",
    permission: "needs permission",
    finished: "finished",
    error: "error",
  };

  function reportForeground() {
    const fg = document.hasFocus();
    void setWindowForeground(fg);
    if (paneId !== null) void setPaneFocus(paneId, fg);
  }

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

    term.onData((data) => {
      if (paneId !== null) void ptyWrite(paneId, data);
    });

    resizeObs = new ResizeObserver(() => {
      if (!fit || !term) return;
      fit.fit();
      if (paneId !== null) void ptyResize(paneId, term.rows, term.cols);
    });
    resizeObs.observe(container);

    unlisteners.push(
      await listen<PaneExitEvent>("pane://exit", (ev) => {
        if (ev.payload.paneId !== paneId || !term) return;
        const s = ev.payload.state;
        const note =
          s.kind === "exited"
            ? `process exited (code ${s.code}${s.signal ? `, ${s.signal}` : ""})`
            : `process ${s.kind}`;
        term.write(`\r\n\x1b[2m[${note}]\x1b[0m\r\n`);
      }),
    );
    unlisteners.push(
      await listen<AttentionEvent>("pane://attention", (ev) => {
        if (ev.payload.paneId !== paneId) return;
        attention = ev.payload.state;
        reason = ev.payload.reason;
      }),
    );

    // Report focus/foreground so the suppression matrix is accurate.
    window.addEventListener("focus", reportForeground);
    window.addEventListener("blur", reportForeground);
    reportForeground();

    term.focus();
  });

  onDestroy(() => {
    window.removeEventListener("focus", reportForeground);
    window.removeEventListener("blur", reportForeground);
    resizeObs?.disconnect();
    unlisteners.forEach((u) => u());
    if (paneId !== null) void closePane(paneId);
    term?.dispose();
  });
</script>

<div
  class="pane"
  class:raised={attention === "raised"}
  class:acknowledged={attention === "acknowledged"}
>
  <div class="terminal" bind:this={container}></div>
  {#if attention === "raised"}
    <div class="badge">{reason ? REASON_LABEL[reason] : "needs you"}</div>
  {/if}
</div>

<style>
  .pane {
    position: relative;
    width: 100%;
    height: 100%;
    background: #0b1020;
  }
  .terminal {
    width: 100%;
    height: 100%;
    padding: 4px;
  }
  /* Attention ring overlay — sits above the terminal, never blocks input. */
  .pane.raised::after,
  .pane.acknowledged::after {
    content: "";
    position: absolute;
    inset: 0;
    border: 2px solid transparent;
    border-radius: 4px;
    pointer-events: none;
  }
  .pane.raised::after {
    border-color: #f5a623;
    box-shadow:
      inset 0 0 12px rgba(245, 166, 35, 0.35),
      0 0 0 1px rgba(245, 166, 35, 0.5);
  }
  .pane.acknowledged::after {
    border-color: rgba(245, 166, 35, 0.25);
  }
  .badge {
    position: absolute;
    top: 6px;
    right: 8px;
    padding: 2px 8px;
    font:
      600 11px/1.4 ui-monospace,
      monospace;
    color: #1a1205;
    background: #f5a623;
    border-radius: 10px;
    pointer-events: none;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.4);
  }
</style>
