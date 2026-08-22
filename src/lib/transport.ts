// The frontend's ONE transport seam (Electron-shell migration U5): every
// backend touchpoint — command invoke, event subscription, pane output
// bytes, window close — routes through here, running on either shell:
//
// - **Tauri**: `@tauri-apps/api` invoke/listen/Channel, exactly as before.
// - **Electron**: the preload's `window.fly` bridge (see electron/preload.cjs)
//   — invoke over the control socket, events fanned out from the backend's
//   broadcast, pane output pushed per-frame with the pane id.
//
// Command names and argument shapes are identical on both paths (KTD1); the
// only structural difference is pane output: Tauri threads a `Channel` through
// `spawn_pane`, Electron subscribes by pane id after spawn resolves — with a
// bounded pre-subscription buffer so early frames are never dropped.

import { invoke as tauriInvoke, Channel } from "@tauri-apps/api/core";
import {
  listen as tauriListen,
  type UnlistenFn as TauriUnlistenFn,
} from "@tauri-apps/api/event";

export type UnlistenFn = () => void;

/** The Electron preload surface (electron/preload.cjs). */
interface FlyBridge {
  invoke(cmd: string, args?: unknown): Promise<unknown>;
  onEvent(cb: (event: string, payload: unknown) => void): () => void;
  onPaneOutput(cb: (paneId: number, bytes: Uint8Array) => void): () => void;
  paneInput(paneId: number, bytes: Uint8Array): void;
  onCloseRequested?(cb: () => void): () => void;
  closeNow?(): void;
}

declare global {
  interface Window {
    fly?: FlyBridge;
  }
}

const bridge: FlyBridge | undefined =
  typeof window !== "undefined" ? window.fly : undefined;

/** True when running inside the Electron shell (U4's `window.fly` bridge). */
export function isElectronShell(): boolean {
  return bridge !== undefined;
}

// ---- invoke -----------------------------------------------------------------

// JSON round-trip before any bridge IPC hop: Electron structured-clones its
// arguments, which throws ("An object could not be cloned") on Svelte 5
// `$state` proxies; Tauri always JSON-serialized (reading through proxies),
// so this exactly preserves the wire semantics the commands were written
// against. EVERY `bridge.invoke` call must pass through this — the resume
// path shipped broken because `spawnPaneWithSink` skipped it and the replayed
// argv arrived as a `$state` proxy.
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
  if (bridge) {
    return bridge.invoke(cmd, plainArgs(args)) as Promise<T>;
  }
  return tauriInvoke<T>(cmd, args);
}

// ---- events -----------------------------------------------------------------
// Electron: one bridge subscription fans out to per-event handler sets, so
// `listen` keeps Tauri's shape (`Promise<UnlistenFn>`, `{ payload }` arg).

type EventHandler = (ev: { payload: unknown }) => void;
const eventHandlers = new Map<string, Set<EventHandler>>();
let bridgeEventsWired = false;

function wireBridgeEvents(): void {
  if (bridgeEventsWired || !bridge) return;
  bridgeEventsWired = true;
  bridge.onEvent((event, payload) => {
    const handlers = eventHandlers.get(event);
    if (!handlers) return;
    for (const h of [...handlers]) h({ payload });
  });
}

export function listen<T>(
  event: string,
  handler: (ev: { payload: T }) => void,
): Promise<UnlistenFn> {
  if (!bridge) return tauriListen<T>(event, handler) as Promise<TauriUnlistenFn>;
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
// id are buffered (bounded) and flushed on registration — the Electron
// analogue of the Channel being wired before spawn under Tauri.

/** Opaque per-pane output sink; construct with [`makeOutputSink`]. */
export interface OutputSink {
  onBytes: (bytes: Uint8Array) => void;
  /** Tauri only: the Channel threaded through `spawn_pane`. */
  channel?: Channel<ArrayBuffer>;
}

const paneSinks = new Map<number, OutputSink>();
const PREBUFFER_CAP = 4 * 1024 * 1024;
const preBuffers = new Map<number, { chunks: Uint8Array[]; total: number }>();
let bridgeOutputWired = false;

function wireBridgeOutput(): void {
  if (bridgeOutputWired || !bridge) return;
  bridgeOutputWired = true;
  bridge.onPaneOutput((paneId, bytes) => {
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
  if (bridge) return { onBytes };
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (message) => {
    onBytes(new Uint8Array(message));
  };
  return { onBytes, channel };
}

/**
 * Spawn a pane with its output sink. Tauri threads the sink's Channel through
 * the command; Electron spawns first, then binds the sink to the returned
 * pane id and flushes any pre-subscription frames in order.
 */
export async function spawnPaneWithSink(
  sink: OutputSink,
  args: Record<string, unknown>,
): Promise<number> {
  if (!bridge) {
    return tauriInvoke<number>("spawn_pane", { channel: sink.channel, ...args });
  }
  wireBridgeOutput();
  const paneId = (await bridge.invoke("spawn_pane", plainArgs(args))) as number;
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
 * of spawning a second one — renderer-crash recovery: the Electron shell
 * reloads the frontend after the Chromium renderer dies, and the core with
 * every agent in it is still running. Null ⇒ nobody owns that leaf: spawn.
 *
 * Electron only. Under Tauri a pane's output Channel is bound at spawn and
 * baked into its coalescer — there is no re-bind, and the Tauri webview is
 * never reloaded in practice — so this answers null without a round-trip
 * (the Tauri command exists for surface parity and answers null too).
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
  if (!bridge) return null;
  wireBridgeOutput();
  const adopted = (await bridge.invoke(
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
// Tauri: getCurrentWindow().onCloseRequested + destroy. Electron: the main
// process intercepts `close` and asks us; `closeNow` finishes the job.

export async function onWindowCloseRequested(
  handler: () => void | Promise<void>,
): Promise<void> {
  if (bridge) {
    bridge.onCloseRequested?.(() => void handler());
    return;
  }
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().onCloseRequested(async (event) => {
    event.preventDefault();
    await handler();
  });
}

/** Actually close the window (after the quit-confirm flow decides). */
export async function destroyWindow(): Promise<void> {
  if (bridge) {
    bridge.closeNow?.();
    return;
  }
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().destroy();
}
