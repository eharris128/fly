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
  import type { ResumeTier } from "./resume";
  import {
    injectionSpawned,
    injectionStep,
    injectionDone,
    injectionPayload,
    isUserInputChunk,
    type InjectionEvent,
    type InjectionState,
  } from "./handoff";
  import "@xterm/xterm/css/xterm.css";
  import {
    spawnPane,
    ptyWrite,
    ptyResize,
    closePane,
    ptyPause,
    ptyResume,
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
    /** Program to run instead of the shell — set only when resuming a Claude
     * agent (U6/U8, KTD-E). Read once at mount; null → a bare shell. */
    command?: string[] | null;
    /** Automations U8/R10: the automation run id this pane serves, threaded to
     * the backend so it links run↔pane atomically at spawn. Read once at mount;
     * null → an ordinary pane. */
    automationRunId?: string | null;
    /** How this pane re-attached (fix-003 U4): "imprecise" (`--continue`,
     * most-recent-in-folder) surfaces a brief disclosure banner so it is never
     * mistaken for an exact re-attach (R5/AE4); "precise"/null shows none. */
    resumeTier?: ResumeTier | null;
    /** Guided session-handoff (session-handoff U3, R9): the stock prompt to
     * pre-type unsent once this pane's composer looks ready. Null for every
     * other pane. Read once at mount (arm time), like `command`. */
    injectText?: string | null;
    /** Fired once when the injection controller reaches a terminal state — or
     * the pane unmounts/fails to spawn first — so App releases the leaf's
     * `guidedHandoffByLeaf` entry (session-handoff U3). */
    onInjectionDone?: (leafKey: string) => void;
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
    command = null,
    automationRunId = null,
    resumeTier = null,
    injectText = null,
    saveScrollback = false,
    onFocusRequest,
    onSpawned,
    onExit,
    onAttention,
    onInjectionDone,
  }: Props = $props();

  // Imprecise-resume disclosure (fix-003 U4, R5/AE4): a pane re-attached via
  // `--continue` shows a brief, dismissible banner naming the most-recent-in-folder
  // fallback so it is never read as an exact re-attach. A precise `--resume <id>`
  // pane needs none — it IS exact. Auto-dismisses; click clears it early.
  let resumeBannerDismissed = $state(false);
  let resumeBannerTimer: ReturnType<typeof setTimeout> | null = null;

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
    alert: "automation alert",
    finished: "finished",
    error: "error",
  };

  // ---- Guided-handoff injection (session-handoff U3, R9) --------------------
  // Thin wiring around the pure reducer in handoff.ts: real timestamps plus a
  // ticker feed injectionStep, and the single `inject` effect writes the
  // bracketed-paste payload straight to the PTY (never through xterm, so it
  // cannot echo back as user input). Focus is NOT touched here — it moved to
  // this pane at spawn (R3); the injection lands whenever readiness does.
  // Ticker resolution only — the reducer's own timings (quiet gap, timeout)
  // are named constants in handoff.ts, where U4 tunes them.
  const INJECT_TICK_MS = 100;
  let injState: InjectionState | null = null; // null = not a guided pane, or done
  let injPayload: string | null = null; // built once at arm time
  let injTicker: ReturnType<typeof setInterval> | null = null;

  function injEvent(ev: InjectionEvent) {
    if (injState === null) return;
    const res = injectionStep(injState, ev);
    injState = res.state;
    if (res.inject && paneId !== null && injPayload !== null)
      void ptyWrite(paneId, injPayload);
    if (injectionDone(res.state)) endInjection();
  }

  // Tear down the ticker and release the App-side registry entry. Runs on any
  // reducer terminal state, on spawn failure, and on unmount (a guided pane
  // closed pre-injection); the null-out makes it idempotent, so the App
  // callback fires at most once.
  function endInjection() {
    if (injTicker !== null) {
      clearInterval(injTicker);
      injTicker = null;
    }
    if (injState !== null) {
      injState = null;
      injPayload = null;
      onInjectionDone?.(leafKey);
    }
  }

  // Flow control (KTD4).
  const HIGH_WATERMARK = 2 * 1024 * 1024;
  const LOW_WATERMARK = 512 * 1024;
  let unacked = 0;
  let paused = false;

  function onOutput(bytes: Uint8Array) {
    if (!term) return;
    // Guided-handoff readiness (U3): every chunk re-arms the quiet gap.
    if (injState !== null) injEvent({ kind: "output", t: Date.now() });
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

  // When this pane becomes the focused leaf, focus xterm. The backend learns
  // "looking" from the visible-pane set App pushes (U17), not per-pane focus.
  $effect(() => {
    if (focused && term) {
      term.focus();
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
    // (R6). Bracketed paste arrives via onData, untouched. On a consumed chord
    // we also preventDefault: otherwise the browser runs the key's default text
    // insertion *after* focus shifts to an overlay it opened (the palette, the
    // rename field), and the character lands in that input.
    term.attachCustomKeyEventHandler((e) => {
      if (keymap && keymap.handle(e)) {
        e.preventDefault();
        return false; // consumed by the app — xterm must not send it either
      }
      return true;
    });

    // Renderer (KTD6): DOM is the default (see `Renderer` in config/schema.rs),
    // so this WebGL path runs only when the user opts in (`auto`/`webgl`). WebGL
    // is not the default because multiple live contexts blank inactive panes on
    // WebKitGTK and the KTD6 per-pane eviction policy was never built; on a
    // context-loss event we dispose the addon, dropping this pane to DOM.
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
    // command is ever re-run (R14, KTD10). Skipped when resuming an agent
    // (command set): the resumed claude TUI repaints the screen, so stale inert
    // scrollback under it would just be noise.
    if (saveScrollback && !command) {
      const prev = await loadScrollback(leafKey);
      if (prev) {
        term.write(prev);
        term.write("\r\n\x1b[2m[previous session]\x1b[0m\r\n");
      }
    }

    // Arm the guided-injection controller before the spawn await so the very
    // first output chunks count toward readiness (session-handoff U3). Stays
    // disarmed (null) for every non-guided pane.
    if (injectText != null) {
      injPayload = injectionPayload(injectText);
      injState = injectionSpawned(Date.now());
      injTicker = setInterval(() => {
        // Ticks gate on the spawn having resolved: only a tick can decide to
        // inject, and the inject writes to the PTY, which needs paneId. The
        // other events flow regardless, and the timeout anchor is spawnedAt,
        // so a deferred first tick still times out on schedule.
        if (paneId === null) return;
        injEvent({ kind: "tick", t: Date.now() });
      }, INJECT_TICK_MS);
    }

    const channel = makeOutputChannel(onOutput);
    // A missing/stale cwd falls back to $HOME (portable-pty filters non-dirs).
    // `command` (resume only) runs a known `claude` invocation instead of the
    // shell — the scoped KTD10 auto-run exception (KTD-E).
    try {
      paneId = await spawnPane(channel, {
        rows,
        cols,
        cwd,
        leafKey,
        command,
        automationRunId,
      });
    } catch (e) {
      // A spawn can be rejected — notably an automation late-link (U8/R10): the
      // run already closed, so the backend refuses to link this pane rather
      // than orphan it. Surface it in the pane instead of leaving a blank one.
      endInjection(); // nothing will ever inject — release the registry entry
      term.write(`\r\n\x1b[31m[failed to start: ${String(e)}]\x1b[0m\r\n`);
      return;
    }
    term.onData((data) => {
      // Guided-handoff U3: a user-originated chunk → the user's intent wins;
      // skip injection. `isUserInputChunk` (handoff.ts) holds the rule: ESC-free
      // chunks count (typed text, Enter, Ctrl-C), ESC-bearing ones don't (xterm
      // answers terminal queries through this same event) — EXCEPT a bracketed
      // paste, which arrives here ESC-wrapped and is user intent. The
      // ESC-prefixed *keyboard* keys (arrows, Esc itself) are covered by onKey
      // below.
      if (injState !== null && isUserInputChunk(data))
        injEvent({ kind: "userInput" });
      if (paneId !== null) void ptyWrite(paneId, data);
    });
    // Guided-handoff U3: any real keystroke is user input — including the
    // ESC-prefixed keys the onData filter above ignores. A leader chord
    // consumed by attachCustomKeyEventHandler never reaches here.
    if (injState !== null) term.onKey(() => injEvent({ kind: "userInput" }));

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
        injEvent({ kind: "paneExit" }); // guided-handoff U3 → Cancelled
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

    // Announce the pane only after its own pane://exit + pane://attention
    // listeners are live, so an attention event emitted synchronously in
    // response to onSpawned — e.g. the U6 alert-sink registration draining its
    // queued rings — is never missed (the listeners filter on the now-known
    // paneId).
    onSpawned?.(leafKey, paneId);

    if (focused) {
      term.focus();
    }

    // Briefly disclose an imprecise resume, then fade it (R5).
    if (resumeTier === "imprecise") {
      resumeBannerTimer = setTimeout(() => (resumeBannerDismissed = true), 10000);
    }
  });

  onDestroy(() => {
    endInjection(); // guided-handoff U3: stop the ticker, release the entry
    if (resumeBannerTimer) clearTimeout(resumeBannerTimer);
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
  {#if resumeTier === "imprecise" && !resumeBannerDismissed}
    <button
      class="resume-banner"
      title="Dismiss"
      onclick={(e) => {
        e.stopPropagation();
        resumeBannerDismissed = true;
      }}
    >
      resumed most-recent session in {cwd ?? "this folder"}
    </button>
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
  /* Imprecise-resume disclosure banner (fix-003 U4): quiet, bottom-left so it
     clears the top-right attention badge; dismissible. */
  .resume-banner {
    position: absolute;
    bottom: 6px;
    left: 8px;
    max-width: calc(100% - 16px);
    padding: 3px 9px;
    font:
      11px/1.4 ui-monospace,
      monospace;
    color: #c9d1d9;
    background: #11182b;
    border: 1px solid #2b3a55;
    border-radius: 10px;
    cursor: pointer;
    opacity: 0.85;
    box-shadow: 0 1px 4px rgba(0, 0, 0, 0.4);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .resume-banner:hover {
    opacity: 1;
    background: #1a2740;
  }
</style>
