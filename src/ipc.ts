// Typed wrappers over the Tauri command + Channel surface (U3).
import { invoke, Channel } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type PaneId = number;

export interface SpawnOpts {
  rows: number;
  cols: number;
  cwd?: string | null;
  /** Stable leaf key, so the backend can key this pane's resume record (U3). */
  leafKey: string;
  /**
   * Program to run instead of the default shell (U6): `command[0]` + args. Set
   * only when resuming a Claude agent (KTD-E); null/undefined → a bare shell.
   */
  command?: string[] | null;
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

/** A recorded notification, emitted when policy says `record` (KTD16, U18). */
export interface NotificationAddedEvent {
  id: number;
  paneId: number;
  reason: AttentionReason;
  title: string | null;
  body: string | null;
  ts: number;
  /** Backend-authored read-at-birth bit (the user was viewing the pane). */
  read: boolean;
}

/**
 * Subscribe to recorded notifications (the first `listen`-based wrapper here;
 * mirrors the `pane://attention` listener pattern in Terminal.svelte). Returns
 * an unlisten fn — the caller tears it down on unmount.
 */
export function onNotificationAdded(
  handler: (ev: NotificationAddedEvent) => void,
): Promise<UnlistenFn> {
  return listen<NotificationAddedEvent>("notification://added", (e) =>
    handler(e.payload),
  );
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
    leafKey: opts.leafKey,
    command: opts.command ?? null,
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

/**
 * The pane's foreground argv when it is a Claude agent, else null (U4). The
 * always-on poll uses this to capture each agent's launch flags for resume.
 */
export function paneCommand(paneId: PaneId): Promise<string[] | null> {
  return invoke<string[] | null>("pane_command", { paneId });
}

/** Upsert a captured agent leaf's launch argv into the resume store (U4). */
export function saveResumeRecord(
  leafKey: string,
  argv: string[],
): Promise<void> {
  return invoke("save_resume_record", { leafKey, argv });
}

/** Prune the resume store to the live layout leaves, dropping orphans (U8). */
export function pruneResumeRecords(liveLeafKeys: string[]): Promise<void> {
  return invoke("prune_resume_records", { liveLeafKeys });
}

/**
 * Per-pane agent state for the dashboard (U4): whether the pane runs a Claude
 * Code agent, its current work stretch (ms; null when idle/not an agent), and
 * how long since its last above-threshold output (ms).
 */
export interface PaneActivity {
  isAgent: boolean;
  workingForMs: number | null;
  lastOutputAgoMs: number | null;
}

/** Poll a pane's agent/work-stretch state (U4). */
export function paneActivity(paneId: PaneId): Promise<PaneActivity> {
  return invoke<PaneActivity>("pane_activity", { paneId });
}

/**
 * One agent leaf's resume mapping (mirrors Rust `ResumeRecord`, U2). `argv` is
 * the captured launch command (the flag source); `sessionCwd` is the project dir
 * the resumed agent runs in (KTD-H). All optional — a record may hold only the
 * hook's session fields, or only the poll's argv, before both writers have run.
 */
export interface ResumeRecord {
  sessionId: string | null;
  sessionCwd: string | null;
  argv: string[] | null;
  isAgent: boolean;
  updatedAt: number;
}

/** Load the write-through resume store, keyed by leaf key (U2/U8). */
export function loadResumeRecords(): Promise<Record<string, ResumeRecord>> {
  return invoke<Record<string, ResumeRecord>>("load_resume_records");
}

/**
 * How the app was launched (mirrors Rust `LaunchMode`, U7):
 *  - `normal` — fresh shells (clean prior exit);
 *  - `resume` — explicit `fly resume`, re-attach agents directly;
 *  - `offer`  — the prior run crashed, so offer to resume.
 */
export type LaunchMode = "normal" | "resume" | "offer";

/** Read how the app was launched, to decide whether to resume (U7/U8). */
export function getLaunchMode(): Promise<LaunchMode> {
  return invoke<LaunchMode>("get_launch_mode");
}

export function saveScrollback(paneKey: string, data: string): Promise<void> {
  return invoke("save_scrollback", { paneKey, data });
}

export function loadScrollback(paneKey: string): Promise<string | null> {
  return invoke<string | null>("load_scrollback", { paneKey });
}
