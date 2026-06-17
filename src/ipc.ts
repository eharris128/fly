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

export type AttentionState = "idle" | "raised" | "acknowledged";
export type AttentionReason = "question" | "permission" | "finished" | "error";
export type AttentionTier = "hook" | "cli" | "bel" | "osc";

export interface AttentionEvent {
  paneId: number;
  state: AttentionState;
  reason: AttentionReason | null;
  tier: AttentionTier | null;
}

/** Replicate a pane's keyboard focus to the backend (KTD8). */
export function setPaneFocus(paneId: PaneId, focused: boolean): Promise<void> {
  return invoke("set_pane_focus", { paneId, focused });
}

/** Replicate the window foreground state to the backend (KTD8). */
export function setWindowForeground(foregrounded: boolean): Promise<void> {
  return invoke("set_window_foreground", { foregrounded });
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

/** Pause a pane's output when unacked bytes exceed the high watermark (KTD4). */
export function ptyPause(paneId: PaneId): Promise<void> {
  return invoke("pty_pause", { paneId });
}

/** Resume a paused pane when unacked bytes drain below the low watermark. */
export function ptyResume(paneId: PaneId): Promise<void> {
  return invoke("pty_resume", { paneId });
}

/** The pane's live working directory (U10/U12). */
export function paneCwd(paneId: PaneId): Promise<string | null> {
  return invoke<string | null>("pane_cwd", { paneId });
}

export function saveScrollback(paneKey: string, data: string): Promise<void> {
  return invoke("save_scrollback", { paneKey, data });
}

export function loadScrollback(paneKey: string): Promise<string | null> {
  return invoke<string | null>("load_scrollback", { paneKey });
}
