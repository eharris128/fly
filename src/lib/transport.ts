// The frontend's ONE transport seam (Electron-shell migration U5): every
// backend touchpoint — command invoke, event subscription, pane output
// bytes, window close — routes through here, over the Electron preload's
// `window.fly` bridge (see electron/preload.cjs): invoke over the control
// socket, events fanned out from the backend's broadcast, pane output pushed
// per-frame with the pane id. Command names and argument shapes are the ones
// `control/registry.rs` pins (KTD1). Pane output subscribes by pane id after
// spawn resolves — with a bounded pre-subscription buffer so early frames are
// never dropped. No other file may touch `window.fly` directly.
//
// (The Tauri branch this seam once carried was retired by
// docs/plans/2026-08-27-001; the bridge is now required, not detected.)

export type UnlistenFn = () => void;

/** The Electron preload surface (electron/preload.cjs). */
interface FlyBridge {
  invoke(cmd: string, args?: unknown): Promise<unknown>;
  onEvent(cb: (event: string, payload: unknown) => void): () => void;
  onPaneOutput(cb: (paneId: number, bytes: Uint8Array) => void): () => void;
  paneInput(paneId: number, bytes: Uint8Array): void;
  onCloseRequested(cb: () => void): () => void;
  closeNow(): void;
}

declare global {
  interface Window {
    fly?: FlyBridge;
  }
}

// Resolved lazily on first use rather than at module load, so importing this
// module in a unit test (or a bare browser tab) never throws — only a real
// transport call without the shell does, with a message that says why.
let cached: FlyBridge | undefined;
function bridge(): FlyBridge {
  if (cached) return cached;
  const b = typeof window !== "undefined" ? window.fly : undefined;
  if (!b) {
    throw new Error(
      "fly: window.fly is missing — the frontend must run inside the Electron shell (electron/), not a bare browser tab",
    );
  }
  cached = b;
  return b;
}

// ---- invoke -----------------------------------------------------------------

// JSON round-trip before any bridge IPC hop: Electron structured-clones its
// arguments, which throws ("An object could not be cloned") on Svelte 5
// `$state` proxies. Commands were written against JSON-serialized args, so
// this exactly preserves their wire semantics. EVERY `bridge().invoke` call
// must pass through this — the resume path shipped broken because
// `spawnPaneWithSink` skipped it and the replayed argv arrived as a `$state`
// proxy (a source-lint test pins the rule).
function plainArgs(
  args?: Record<string, unknown>,
): Record<string, unknown> | undefined {
  return args === undefined
    ? undefined
    : (JSON.parse(JSON.stringify(args)) as Record<string, unknown>);
}

export function invoke<T = void>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return bridge().invoke(cmd, plainArgs(args)) as Promise<T>;
}

// ---- events -----------------------------------------------------------------
// One bridge subscription fans out to per-event handler sets; `listen` keeps
// the `Promise<UnlistenFn>` / `{ payload }` shape the callers were written
// against.

type EventHandler = (ev: { payload: unknown }) => void;
const eventHandlers = new Map<string, Set<EventHandler>>();
let bridgeEventsWired = false;

function wireBridgeEvents(): void {
  if (bridgeEventsWired) return;
  bridgeEventsWired = true;
  bridge().onEvent((event, payload) => {
    const handlers = eventHandlers.get(event);
    if (!handlers) return;
    for (const h of [...handlers]) h({ payload });
  });
}

export function listen<T>(
  event: string,
  handler: (ev: { payload: T }) => void,
): Promise<UnlistenFn> {
  wireBridgeEvents();
  let set = eventHandlers.get(event);
  if (!set) {
    set = new Set();
    eventHandlers.set(event, set);
  }
  set.add(handler as EventHandler);
  return Promise.resolve(() => {
    set.delete(handler as EventHandler);
  });
}

// ---- pane output ------------------------------------------------------------
// One bridge subscription; per-pane sinks registered on spawn. Frames that
// arrive between the backend spawning the pane and the renderer learning its
// id are buffered (bounded) and flushed on registration.

/** Opaque per-pane output sink; construct with [`makeOutputSink`]. */
export interface OutputSink {
  onBytes: (bytes: Uint8Array) => void;
}

const paneSinks = new Map<number, OutputSink>();
const PREBUFFER_CAP = 4 * 1024 * 1024;
const preBuffers = new Map<number, { chunks: Uint8Array[]; total: number }>();
let bridgeOutputWired = false;

function wireBridgeOutput(): void {
  if (bridgeOutputWired) return;
  bridgeOutputWired = true;
  bridge().onPaneOutput((paneId, bytes) => {
    const sink = paneSinks.get(paneId);
    if (sink) {
      sink.onBytes(bytes);
      return;
    }
    let buf = preBuffers.get(paneId);
    if (!buf) {
      buf = { chunks: [], total: 0 };
      preBuffers.set(paneId, buf);
    }
    if (buf.total + bytes.length <= PREBUFFER_CAP) {
      buf.chunks.push(bytes);
      buf.total += bytes.length;
    }
    // Over cap: drop silently — a pane nobody subscribes to is a leak
    // guard, not a data path (subscription follows spawn within a tick).
  });
}

export function makeOutputSink(onBytes: (bytes: Uint8Array) => void): OutputSink {
  return { onBytes };
}

/**
 * Spawn a pane with its output sink: spawn first, then bind the sink to the
 * returned pane id and flush any pre-subscription frames in order.
 */
export async function spawnPaneWithSink(
  sink: OutputSink,
  args: Record<string, unknown>,
): Promise<number> {
  wireBridgeOutput();
  const paneId = (await bridge().invoke("spawn_pane", plainArgs(args))) as number;
  paneSinks.set(paneId, sink);
  const buffered = preBuffers.get(paneId);
  if (buffered) {
    preBuffers.delete(paneId);
    for (const chunk of buffered.chunks) sink.onBytes(chunk);
  }
  return paneId;
}

/** The `adopt_live_pane` answer (stream.rs `AdoptedPane`, camelCase). */
export interface AdoptedPane {
  paneId: number;
  /** The pane's current grid — size the xterm to it BEFORE writing `tail`. */
  rows: number;
  cols: number;
  /** The pane's raw-output tail ring (≤ 64 KiB), for the initial paint. */
  tail: string;
  attention: "idle" | "raised" | "acknowledged";
  reason: string | null;
}

/**
 * Re-attach to the LIVE pane the backend still owns for `leafKey` instead
 * of spawning a second one — renderer-crash recovery: the shell reloads the
 * frontend after the Chromium renderer dies, and the core with every agent
 * in it is still running. Null ⇒ nobody owns that leaf: spawn.
 *
 * Capture-then-subscribe: frames that reached the bridge before the sink
 * binds predate (or overlap) the tail snapshot, so they are DISCARDED — a
 * few ms of output in the gap can be lost, never duplicated (the tmux U8
 * adopt replay makes the same call, for the same reason).
 */
export async function adoptLivePaneWithSink(
  sink: OutputSink,
  args: { leafKey: string },
): Promise<AdoptedPane | null> {
  wireBridgeOutput();
  const adopted = (await bridge().invoke(
    "adopt_live_pane",
    plainArgs(args),
  )) as AdoptedPane | null;
  if (!adopted) return null;
  preBuffers.delete(adopted.paneId);
  paneSinks.set(adopted.paneId, sink);
  return adopted;
}

/** Drop a pane's sink (close path) so a reused map entry can't leak. */
export function releasePaneSink(paneId: number): void {
  paneSinks.delete(paneId);
  preBuffers.delete(paneId);
}

// ---- window close -----------------------------------------------------------
// The main process intercepts `close` and asks us; `closeNow` finishes the
// job once the quit-confirm flow decides.

export async function onWindowCloseRequested(
  handler: () => void | Promise<void>,
): Promise<void> {
  bridge().onCloseRequested(() => void handler());
}

/** Actually close the window (after the quit-confirm flow decides). */
export async function destroyWindow(): Promise<void> {
  bridge().closeNow();
}
