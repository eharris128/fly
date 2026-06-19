<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { Terminal } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import { WebglAddon } from "@xterm/addon-webgl";
  import { Unicode11Addon } from "@xterm/addon-unicode11";
  import { SerializeAddon } from "@xterm/addon-serialize";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getConfig } from "./config";
  import type { Keymap } from "./keymap";
  import "@xterm/xterm/css/xterm.css";
  import {
    spawnPane,
    ptyWrite,
    ptyResize,
    closePane,
    ptyPause,
    ptyResume,
    setPaneFocus,
    makeOutputChannel,
    loadScrollback,
    saveScrollback as persistScrollback,
    type PaneId,
    type PaneExitEvent,
    type AttentionEvent,
    type AttentionState,
    type AttentionReason,
  } from "../ipc";

  interface Props {
    leafKey: string;
    focused: boolean;
    keymap?: Keymap | null;
    cwd?: string | null;
    saveScrollback?: boolean;
    onFocusRequest: (leafKey: string) => void;
    onSpawned?: (leafKey: string, paneId: PaneId) => void;
    onExit?: (leafKey: string) => void;
    onAttention?: (
      leafKey: string,
      state: AttentionState,
      reason: AttentionReason | null,
    ) => void;
  }
  let {
    leafKey,
    focused,
    keymap,
    cwd = null,
    saveScrollback = false,
    onFocusRequest,
    onSpawned,
    onExit,
    onAttention,
  }: Props = $props();

  let serializer: SerializeAddon | undefined;

  let container: HTMLDivElement;
  let term: Terminal | undefined;
  let fit: FitAddon | undefined;
  let paneId: PaneId | null = null;
  let unlisteners: UnlistenFn[] = [];
  let resizeObs: ResizeObserver | null = null;

  let attention = $state<AttentionState>("idle");
  let reason = $state<AttentionReason | null>(null);

  const REASON_LABEL: Record<AttentionReason, string> = {
    question: "waiting for you",
    permission: "needs permission",
    finished: "finished",
    error: "error",
  };

  // Flow control (KTD4).
  const HIGH_WATERMARK = 2 * 1024 * 1024;
  const LOW_WATERMARK = 512 * 1024;
  let unacked = 0;
  let paused = false;

  function onOutput(bytes: Uint8Array) {
    if (!term) return;
    const n = bytes.length;
    unacked += n;
    term.write(bytes, () => {
      unacked -= n;
      if (paused && unacked < LOW_WATERMARK && paneId !== null) {
        paused = false;
        void ptyResume(paneId);
      }
    });
    if (!paused && unacked > HIGH_WATERMARK && paneId !== null) {
      paused = true;
      void ptyPause(paneId);
    }
  }

  // When this pane becomes the focused leaf, focus xterm and tell the backend.
  $effect(() => {
    if (focused && term && paneId !== null) {
      term.focus();
      void setPaneFocus(paneId, document.hasFocus());
    }
  });

  // Imperatively focus this pane's terminal. The command palette uses this to
  // hand focus back to the active pane when it closes — the palette takes DOM
  // focus (the cheat-sheet never does, KTD3), so without this the leader would
  // go dead until the user clicked a pane.
  export function focus() {
    term?.focus();
  }

  onMount(async () => {
    const config = await getConfig();

    term = new Terminal({
      fontFamily: "ui-monospace, 'JetBrains Mono', Menlo, Consolas, monospace",
      fontSize: config.fontSize,
      cursorBlink: true,
      scrollback: config.scrollbackLines,
      allowProposedApi: true,
      theme: { background: "#0b1020", foreground: "#c9d1d9" },
    });

    fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new Unicode11Addon());
    serializer = new SerializeAddon();
    term.loadAddon(serializer);
    term.unicode.activeVersion = "11";
    term.open(container);

    // The leader chord is intercepted here and never reaches the shell; every
    // other key (Ctrl-C, Ctrl-W, vim nav, …) returns true and flows to the PTY
    // (R6). Bracketed paste arrives via onData, untouched.
    term.attachCustomKeyEventHandler((e) => (keymap ? !keymap.handle(e) : true));

    if (config.renderer !== "dom") {
      try {
        const webgl = new WebglAddon();
        webgl.onContextLoss(() => webgl.dispose());
        term.loadAddon(webgl);
      } catch (err) {
        console.warn("WebGL renderer unavailable, using DOM renderer", err);
      }
    }

    fit.fit();
    const cols = term.cols >= 2 ? term.cols : 80;
    const rows = term.rows >= 2 ? term.rows : 24;

    // Replay prior scrollback as inert text before the live shell starts — no
    // command is ever re-run (R14, KTD10).
    if (saveScrollback) {
      const prev = await loadScrollback(leafKey);
      if (prev) {
        term.write(prev);
        term.write("\r\n\x1b[2m[previous session]\x1b[0m\r\n");
      }
    }

    const channel = makeOutputChannel(onOutput);
    // A missing/stale cwd falls back to $HOME (portable-pty filters non-dirs).
    paneId = await spawnPane(channel, { rows, cols, cwd });
    onSpawned?.(leafKey, paneId);

    term.onData((data) => {
      if (paneId !== null) void ptyWrite(paneId, data);
    });

    resizeObs = new ResizeObserver(() => {
      if (!fit || !term) return;
      // Skip while hidden (a background tab) — clientWidth is 0.
      if (container.clientWidth === 0 || container.clientHeight === 0) return;
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
        onExit?.(leafKey);
      }),
    );
    unlisteners.push(
      await listen<AttentionEvent>("pane://attention", (ev) => {
        if (ev.payload.paneId !== paneId) return;
        attention = ev.payload.state;
        reason = ev.payload.reason;
        onAttention?.(leafKey, ev.payload.state, ev.payload.reason);
      }),
    );

    if (focused) {
      void setPaneFocus(paneId, document.hasFocus());
      term.focus();
    }
  });

  onDestroy(() => {
    resizeObs?.disconnect();
    unlisteners.forEach((u) => u());
    // Persist scrollback if opted in (off by default for privacy, KTD10).
    if (saveScrollback && serializer && term) {
      void persistScrollback(leafKey, serializer.serialize());
    }
    if (paneId !== null) void closePane(paneId);
    term?.dispose();
  });
</script>

<div
  class="pane"
  class:raised={attention === "raised"}
  class:acknowledged={attention === "acknowledged"}
  class:focused
  role="presentation"
  onpointerdown={() => onFocusRequest(leafKey)}
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
  /* A subtle focus outline so the active pane is obvious. */
  .pane.focused::before {
    content: "";
    position: absolute;
    inset: 0;
    border: 1px solid #2b3a55;
    pointer-events: none;
  }
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
