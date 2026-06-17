// Typed wrappers over the Tauri command + Channel surface (U3).
import { invoke, Channel } from "@tauri-apps/api/core";

export type PaneId = number;

export interface SpawnOpts {
  rows: number;
  cols: number;
  cwd?: string | null;
}

/** Lifecycle state as serialized by the Rust `LifecycleState` enum. */
export type LifecycleState =
  | { kind: "spawning" }
  | { kind: "live" }
  | { kind: "exited"; code: number; signal: string | null }
  | { kind: "killed" }
  | { kind: "failed"; error: string }
  | { kind: "restored_inert" };

export interface PaneExitEvent {
  paneId: number;
  state: LifecycleState;
}

/**
 * Create an output Channel that decodes raw PTY bytes to `Uint8Array`.
 * The backend sends `InvokeResponseBody::Raw`, which arrives as an
 * `ArrayBuffer` — raw bytes end-to-end, no transcoding (KTD3).
 */
export function makeOutputChannel(
  onBytes: (bytes: Uint8Array) => void,
): Channel<ArrayBuffer> {
  const channel = new Channel<ArrayBuffer>();
  channel.onmessage = (message) => {
    onBytes(new Uint8Array(message));
  };
  return channel;
}

export function spawnPane(
  channel: Channel<ArrayBuffer>,
  opts: SpawnOpts,
): Promise<PaneId> {
  return invoke<PaneId>("spawn_pane", {
    channel,
    rows: opts.rows,
    cols: opts.cols,
    cwd: opts.cwd ?? null,
  });
}

export function ptyWrite(paneId: PaneId, data: string): Promise<void> {
  return invoke("pty_write", { paneId, data });
}

export function ptyResize(
  paneId: PaneId,
  rows: number,
  cols: number,
): Promise<void> {
  return invoke("pty_resize", { paneId, rows, cols });
}

export function closePane(paneId: PaneId): Promise<void> {
  return invoke("close_pane", { paneId });
}
