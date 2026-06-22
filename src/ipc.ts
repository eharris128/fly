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

/**
 * Replicate the set of visible panes — the active tab's leaves in the active
 * workspace (U17). Generalizes the old per-pane keyboard-focus replication: any
 * visible pane counts as "looking" for the Acknowledged transition.
 */
export function setVisiblePanes(paneIds: PaneId[]): Promise<void> {
  return invoke("set_visible_panes", { paneIds });
}

/** Replicate the window foreground state to the backend (KTD8). */
export function setWindowForeground(foregrounded: boolean): Promise<void> {
  return invoke("set_window_foreground", { foregrounded });
}

/** Replicate whether the notification panel is open (desktop suppressor, KTD15). */
export function setPanelOpen(open: boolean): Promise<void> {
  return invoke("set_panel_open", { open });
}

/** Toggle global do-not-disturb (R17). */
export function setMuted(muted: boolean): Promise<void> {
  return invoke("set_muted", { muted });
}

/** Mute or unmute a single workspace (R18). */
export function setWorkspaceMuted(
  workspace: string,
  muted: boolean,
): Promise<void> {
  return invoke("set_workspace_muted", { workspace, muted });
}

/** Tell the backend which workspace a pane belongs to, for mute scoping (U17). */
export function setPaneWorkspace(
  paneId: PaneId,
  workspace: string,
): Promise<void> {
  return invoke("set_pane_workspace", { paneId, workspace });
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
