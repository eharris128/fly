// Typed wrappers over the backend command surface (U3): every invoke/listen
// routes through `lib/transport.ts` (the Electron preload bridge, U5).
import {
  invoke,
  listen,
  makeOutputSink,
  releasePaneSink,
  spawnPaneWithSink,
  type OutputSink,
  type UnlistenFn,
  adoptLivePaneWithSink,
  type AdoptedPane as TransportAdoptedPane,
} from "./lib/transport";
// Type-only (erased at runtime, so the feed.ts ↔ ipc.ts cycle is harmless).
import type { FeedPublishPayload } from "./lib/feed";
import { makeWriteChain } from "./lib/write-chain";

export type PaneId = number;

/**
 * Forward a frontend message to the app's stderr (the webview console is
 * invisible outside devtools). Mirrors the global handler in `main.ts`; used on
 * recoverable error paths that still want a breadcrumb.
 */
export function frontendLog(msg: string): Promise<void> {
  return invoke("frontend_log", { msg });
}

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
  /**
   * Automations U8/R10: the automation run id this pane serves. When set, the
   * backend links run↔pane atomically before the child spawns (sets
   * `RunRow.pane_id`, marks the recursion registry). A late link — the run
   * already closed — rejects the spawn. Null/undefined for an ordinary pane.
   */
  automationRunId?: string | null;
  /** tmux-substrate U10: an ephemeral pane (automation run tab, alerts sink)
   * must NOT survive quit — its tmux session is killed at close_all, never
   * detached, so ephemeral tabs can't accumulate orphaned marked sessions. */
  ephemeral?: boolean;
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
/**
 * Why a pane is raised (mirrors Rust `Reason`). "alert" is the automations
 * alert reason (automations U-ID U12, R18/KTD-H) — the first non-agent
 * producer; any pane may also send it via `fly notify` and it is valid.
 */
export type AttentionReason =
  | "question"
  | "permission"
  | "finished"
  | "error"
  | "alert";
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
 * An automation agent run to launch (automations U7/U8, R9/R10). Emitted by the
 * backend `AgentDispatcher` after the run is claimed + persisted; the frontend
 * creates a background tab running `claude <prompt>` in `cwd`, placed in the
 * origin workspace (or the first, R9 fallback), and calls {@link spawnPane}
 * with `automationRunId = runId` so the backend links run↔pane atomically.
 */
export interface AgentRunEvent {
  runId: string;
  name: string;
  prompt: string;
  cwd: string;
  /** Workspace identity resolved at create time (not a pane id). Superseded by
   * the durable Automations-workspace role marker (U6/U7); kept on the wire. */
  originWorkspaceHint: string;
  /** Resolved launch flags (U4a, R11). The frontend appends `--model` /
   * `--effort` / `--fallback-model` only when the matching value is non-null. */
  model: string | null;
  effort: string | null;
  fallbackModel: string | null;
}

/**
 * Subscribe to automation agent-run dispatches (`automation://agent-run`).
 * Returns an unlisten fn — the caller (App.svelte) tears it down on unmount.
 */
export function onAgentRun(
  handler: (ev: AgentRunEvent) => void,
): Promise<UnlistenFn> {
  return listen<AgentRunEvent>("automation://agent-run", (e) =>
    handler(e.payload),
  );
}

/**
 * An automation **agent** run reached a terminal status (automations-workspace-
 * and-model U5). Emitted after the run's store mutation, so the frontend tab
 * lifecycle (U8) can auto-close a `succeeded` run's tab after a linger, or keep
 * a `failed` one for review.
 */
export interface RunClosedEvent {
  runId: string;
  automationId: string;
  status: RunStatus;
}

/**
 * Subscribe to agent-run closes (`automation://run-closed`). Returns an unlisten
 * fn — the caller (App.svelte) tears it down on unmount.
 */
export function onRunClosed(
  handler: (ev: RunClosedEvent) => void,
): Promise<UnlistenFn> {
  return listen<RunClosedEvent>("automation://run-closed", (e) =>
    handler(e.payload),
  );
}

/**
 * A monitor automation was registered from inside pane `paneId` and persisted
 * as `automationId` (monitor-handoff U4, mirrors Rust `MonitorRegisteredEvent`).
 * Emitted strictly after the store flush, so by the time this arrives the
 * handoff is durable — the registering pane's tab is residue and the frontend
 * (U6, R13) closes it immediately, no linger.
 */
export interface MonitorRegisteredEvent {
  paneId: number;
  automationId: string;
}

/**
 * Subscribe to monitor registrations (`automation://monitor-registered`).
 * Returns an unlisten fn — the caller (App.svelte) tears it down on unmount.
 */
export function onMonitorRegistered(
  handler: (ev: MonitorRegisteredEvent) => void,
): Promise<UnlistenFn> {
  return listen<MonitorRegisteredEvent>("automation://monitor-registered", (e) =>
    handler(e.payload),
  );
}

/**
 * An automation alert with no sink pane yet (automations U6/R17). Emitted by the
 * backend when an alert-classified script run concludes and the "Automations"
 * sink pane isn't registered. The frontend single-flights a background ephemeral
 * tab that `tail -f`s {@link AlertPendingEvent.logPath}, then calls
 * {@link registerAlertSink} with the new pane id so the backend drains the queued
 * alerts and rings the pane (R18).
 */
export interface AlertPendingEvent {
  /** Absolute path to the alerts log the sink pane should tail. */
  logPath: string;
}

/**
 * Subscribe to alert-pending events (`automation://alert-pending`). Returns an
 * unlisten fn — the caller (App.svelte) tears it down on unmount.
 */
export function onAlertPending(
  handler: (ev: AlertPendingEvent) => void,
): Promise<UnlistenFn> {
  return listen<AlertPendingEvent>("automation://alert-pending", (e) =>
    handler(e.payload),
  );
}

/**
 * Register the "Automations" sink pane (automations U6/R17): the backend records
 * the pane id, drains any alerts queued before it existed, and rings it once per
 * drained alert (R18). Called from App.svelte once the sink pane mounts.
 */
export function registerAlertSink(paneId: PaneId): Promise<void> {
  return invoke("register_alert_sink", { paneId });
}

// ---- automations dashboard (U10, R25/R6) ------------------------------------
// TS mirrors of the Rust `automations::model` types (serde camelCase, the shape
// that already crosses the store file, the socket, and now the dashboard). The
// pure view-model in `lib/automations.ts` turns these into rendered rows.

/** Run-row mode discriminant (`RunMode` in model.rs) — also an automation's mode. */
export type AutomationMode = "agent" | "script";
/** Run-row status (`RunStatus` in model.rs). `running` is the only
 * non-terminal. `withheld` (automation-dependencies R2) is a dependent's
 * honest decline: its upstream didn't qualify, reason in `error`. */
export type RunStatus = "running" | "succeeded" | "failed" | "skipped" | "withheld";
/** What started a run (`Trigger` in model.rs). `retry` is a one-shot re-run of a
 * run an app crash/restart interrupted (interrupt-resilience U1). */
export type RunTrigger = "schedule" | "manual" | "retry";

/**
 * The machine-readable monitor verdict (mirrors Rust `Verdict`,
 * monitor-handoff U1/R2): the pass/fail outcome plus the free-text note the
 * backend parsed from the check's final assistant turn. The note is captured
 * agent output — untrusted for display (sanitize before rendering).
 */
export interface Verdict {
  outcome: "pass" | "fail";
  note: string;
}

/**
 * Pickup pointers captured at monitor registration (mirrors Rust
 * `MonitorPointers`, monitor-handoff U1/R11): the parent session's id,
 * transcript path, and cwd — what the U7 pickup action spawns against (R16),
 * after validating the transcript/cwd still exist (R17).
 */
export interface MonitorPointers {
  sessionId: string;
  transcriptPath: string;
  sessionCwd: string;
}

/** One bounded run-history row (mirrors Rust `RunRow`, R8). */
export interface RunRow {
  id: string;
  mode: AutomationMode;
  trigger: RunTrigger;
  status: RunStatus;
  /** Linked pane for agent runs (R10); null for scripts / unlinked rows. */
  paneId: number | null;
  /** Model / reasoning effort this agent run launched with (automations-
   * workspace-and-model U1/U4a, R13); null for scripts and for runs that used
   * Claude's own default. */
  model: string | null;
  effort: string | null;
  /** Parsed monitor verdict (monitor-handoff U1/R2); null for every
   * non-monitor run and for checks that parsed no verdict (R5). */
  verdict: Verdict | null;
  /** Durable failure-bundle path for a FAIL verdict (monitor-handoff R15);
   * null otherwise (including a PASS, or a failed bundle write). */
  bundlePath: string | null;
  /** Closed-loop dispatch marker (headless-monitor-checks U2, widened by
   * headless-agent-automations R1): true when this run is a backend-owned
   * `claude -p` child — no pane, no tab. */
  headless: boolean;
  /** The headless run's Claude session id (stamped at close from the stream's
   * init event); absent/null for pane runs (headless-agent-automations R10). */
  sessionId?: string | null;
  /** The upstream run a dependent's fire consumed (automation-dependencies
   * R3/KTD3); absent/null for every non-dependent run. */
  upstreamRunId?: string | null;
  output: string | null;
  exitCode: number | null;
  /** Failure detail for `failed` rows; the skip reason for `skipped` rows. */
  error: string | null;
  scheduledFor: number | null;
  startedAt: number | null;
  finishedAt: number | null;
}

/** What a due automation executes (serde-tagged `Mode` in model.rs). */
export type AutomationSpec =
  | {
      kind: "agent";
      prompt: string;
      /** Pinned launch model + reasoning effort (U1/U2, R9/R10); null ⇒ shared
       * default / Claude's own default resolved at dispatch. */
      model: string | null;
      effort: string | null;
      /** Dispatch disposition (headless-agent-automations R1): true =
       * `--headless`, false = `--paned`, null = follow the config default
       * (`AutomationsDashboard.headlessDefault`) at claim time. */
      headless: boolean | null;
    }
  | { kind: "script"; scriptFile: string; interpreter: string; timeoutMs: number };

/** Where an automation came from (mirrors Rust `Origin`, R22/R9). */
export interface AutomationOrigin {
  paneId: number;
  workspaceId: string;
  label: string;
}

/** A scheduled automation with its embedded run history (mirrors Rust `Automation`). */
export interface Automation {
  id: string;
  name: string;
  cron: string;
  timezone: string;
  enabled: boolean;
  /** Opt-in interrupt resilience (interrupt-resilience U1/R1): re-run once on the
   * next launch if an app crash/restart interrupts a run. Default false. */
  retryOnInterrupt: boolean;
  /** Monitor flavor (monitor-handoff U1/R1): an agent-mode automation with a
   * not-before time that delivers one machine-readable verdict and retires. */
  monitor: boolean;
  /** Epoch-ms floor below which a monitor never runs (monitor-handoff R1);
   * null for non-monitors and floor-less monitors. */
  notBeforeMs: number | null;
  /** Set (epoch ms) when a parsed verdict retired this monitor (monitor-handoff
   * R3/R4): scheduling stopped permanently, record and history kept. */
  retiredAt: number | null;
  /** Pickup pointers captured at registration (monitor-handoff R11); null for
   * every non-monitor automation. */
  pickupPointers: MonitorPointers | null;
  /** Dependency edge (automation-dependencies R1): this automation's due
   * occurrences fire only against a fresh, successful, not-yet-consumed run
   * of `upstreamId`; null/absent for ordinary automations. */
  after?: { upstreamId: string; withinMs?: number | null } | null;
  cwd: string;
  mode: AutomationSpec;
  origin: AutomationOrigin;
  createdAt: number;
  updatedAt: number;
  /** Next occurrence in epoch ms; null = paused (R23). */
  nextRunAt: number | null;
  runs: RunRow[];
}

/**
 * The dashboard payload (mirrors Rust `AutomationsDashboard`): the raw list
 * plus flattened store health for the R6 warning row. `degraded` is the
 * at-a-glance bit; `corruptBak` names where corrupt store bytes were preserved;
 * `flushError` carries a failing-flush detail. Both null when healthy.
 * `infraFailures` / `monitorBrokenThreshold` (monitor-handoff U7, R18) are the
 * backend-derived broken-monitor inputs: the per-monitor consecutive
 * infra-failure count and the one Rust threshold constant, so the frontend
 * never re-derives the walk or hardcodes the number.
 */
export interface AutomationsDashboard {
  automations: Automation[];
  /** The config dispatch-disposition default (headless-agent-automations R9):
   * `automationDefaults.headless`, so the panel resolves each automation's
   * effective disposition exactly as the claim does. */
  headlessDefault: boolean;
  /** Monitor id → derived consecutive-infra-failure count (monitors only). */
  infraFailures: Record<string, number>;
  /** Mirrors Rust `verdict::MONITOR_BROKEN_THRESHOLD`. */
  monitorBrokenThreshold: number;
  degraded: boolean;
  corruptBak: string | null;
  flushError: string | null;
}

/** Fetch the automations + store health for the dashboard panel (U10). */
export function listAutomations(): Promise<AutomationsDashboard> {
  return invoke<AutomationsDashboard>("list_automations");
}

/**
 * Delete an automation by id — the dashboard row's ✕, mirroring `fly
 * automation delete` (R23 teardown: run history removed, an in-flight run
 * stopped). The manager emits `automation://changed` on success so the panel
 * refetches itself; rejects with a one-line reason for an unknown id (a raced
 * CLI delete).
 */
export function deleteAutomation(id: string): Promise<void> {
  return invoke("delete_automation", { id });
}

/**
 * R17 pickup validation (monitor-handoff U7, mirrors Rust `PickupCheck`):
 * whether a failed monitor's stored transcript path and session cwd still
 * exist on disk. Read-only metadata check — the pickup button decides
 * spawn-vs-fallback on this, never on a broken `claude` launch.
 */
export interface PickupCheck {
  transcriptExists: boolean;
  cwdExists: boolean;
}

/** Check a pickup target's transcript + cwd existence (monitor-handoff R17). */
export function monitorPickupCheck(
  transcriptPath: string,
  cwd: string,
): Promise<PickupCheck> {
  return invoke<PickupCheck>("monitor_pickup_check", { transcriptPath, cwd });
}

/**
 * Read a monitor failure bundle's text for the R17 fallback surface
 * (monitor-handoff U7). Backend-scoped to the monitor-bundles dir (anything
 * else rejects) and display-capped; rejects with a one-line reason. The text
 * is captured agent output — sanitize before rendering.
 */
export function readMonitorBundle(path: string): Promise<string> {
  return invoke<string>("read_monitor_bundle", { path });
}

/**
 * Subscribe to automation mutations (`automation://changed`; payload is the
 * changed automation's id). The dashboard refetches on each; returns an unlisten
 * fn the caller tears down on unmount.
 */
export function onAutomationChanged(
  handler: (id: string) => void,
): Promise<UnlistenFn> {
  return listen<string>("automation://changed", (e) => handler(e.payload));
}

/**
 * Tell the backend the frontend has finished restore and is listening for
 * agent-run events (automations R5). Until this fires, the sweep defers due
 * *agent* automations (script automations run regardless) so a dispatch never
 * fires into a listener-less void and burns the occurrence.
 */
export function automationsFrontendReady(): Promise<void> {
  return invoke("automations_frontend_ready");
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
): OutputSink {
  return makeOutputSink(onBytes);
}

export function spawnPane(
  sink: OutputSink,
  opts: SpawnOpts,
): Promise<PaneId> {
  return spawnPaneWithSink(sink, {
    rows: opts.rows,
    cols: opts.cols,
    cwd: opts.cwd ?? null,
    leafKey: opts.leafKey,
    command: opts.command ?? null,
    automationRunId: opts.automationRunId ?? null,
    ephemeral: opts.ephemeral ?? false,
  });
}

/** A re-attached live pane (renderer-crash recovery) — `adopt_live_pane`'s
 * answer with the attention fields typed as the rest of this module types
 * them. */
export interface AdoptedPane extends Omit<TransportAdoptedPane, "attention" | "reason"> {
  attention: AttentionState;
  reason: AttentionReason | null;
}

/**
 * Re-attach to the live pane the backend still owns for a leaf, binding the
 * output sink to its existing id (no spawn). Null when no live pane owns the
 * leaf, so the caller spawns as usual. The answer carries the pane's current grid (size
 * the xterm to it first) and its output tail for the first paint. See
 * `transport.adoptLivePaneWithSink`.
 */
export function adoptLivePane(
  sink: OutputSink,
  opts: { leafKey: string },
): Promise<AdoptedPane | null> {
  return adoptLivePaneWithSink(sink, opts) as Promise<AdoptedPane | null>;
}

/**
 * Per-pane write serialization (poll-batching plan KTD5/R5). `pty_write` is an
 * async backend command, so two in-flight invokes can complete out of order
 * across runtime workers — keystroke order is pinned here instead: each pane's
 * write starts only after the previous one settles (see `lib/write-chain.ts`).
 * A failed write rejects its own caller but never wedges the chain.
 */
const paneWrites = makeWriteChain<PaneId>();

/** U7 (tmux-substrate KTD6): open the pane's tmux session in a real
 * terminal. Backend refuses for PTY-backed panes (substrate off). */
export function attachPane(paneId: PaneId): Promise<void> {
  return invoke("attach_pane", { paneId });
}

export function ptyWrite(paneId: PaneId, data: string): Promise<void> {
  return paneWrites.run(paneId, () => invoke<void>("pty_write", { paneId, data }));
}

export function ptyResize(
  paneId: PaneId,
  rows: number,
  cols: number,
): Promise<void> {
  return invoke("pty_resize", { paneId, rows, cols });
}

export function closePane(paneId: PaneId): Promise<void> {
  releasePaneSink(paneId);
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

/**
 * The pane's active Claude session id from the transcript store, else null
 * (fix-resume-session-selection U1). The always-on poll reads this to capture
 * each agent's precise session id without depending on the installed `fly`
 * binary's version (KTD-A).
 */
export function paneSessionId(paneId: PaneId): Promise<string | null> {
  return invoke<string | null>("pane_session_id", { paneId });
}

/** Upsert a captured agent leaf's launch argv into the resume store (U4). */
export function saveResumeRecord(
  leafKey: string,
  argv: string[],
): Promise<void> {
  return invoke("save_resume_record", { leafKey, argv });
}

/**
 * Upsert a captured agent leaf's active session id (+ its cwd) into the resume
 * store (fix-resume-session-selection U2). The poll calls this when a Claude
 * pane's transcript-derived session id first appears or changes, keeping the
 * stored id current across `/clear` and new conversations (KTD-B). Field-merging,
 * so it never clobbers the leaf's captured argv.
 */
export function saveResumeSession(
  leafKey: string,
  sessionId: string,
  sessionCwd: string | null,
): Promise<ResumeRecord> {
  return invoke<ResumeRecord>("save_resume_session", {
    leafKey,
    sessionId,
    sessionCwd,
  });
}

/**
 * Bind a user-picked session to a leaf at the highest trust rank
 * (fix-session-pane-attribution U6, R10/KTD2). Superseded only by another
 * explicit pick or a reset; a hook reporting a divergent live id flags
 * `divergencePending` but never rebinds. Returns the effective stored record
 * so the pick flow can confirm the bind.
 */
export function saveSessionPick(
  leafKey: string,
  sessionId: string,
  sessionCwd: string | null,
): Promise<ResumeRecord> {
  return invoke<ResumeRecord>("save_session_pick", {
    leafKey,
    sessionId,
    sessionCwd,
  });
}

/** Prune the resume store to the live layout leaves, dropping orphans (U8). */
export function pruneResumeRecords(liveLeafKeys: string[]): Promise<void> {
  return invoke("prune_resume_records", { liveLeafKeys });
}

/**
 * Clear a leaf's session attribution — id, source, divergence flag — leaving
 * the poll's argv/isAgent intact (fix-session-pane-attribution U8, KTD7/R14).
 * The user escape valve for a stranded, stale, or diverged precise id that no
 * automatic writer may correct: resolution then returns empty and the next
 * launch re-captures via the pick-list (which includes aged-out targets, so
 * reset is non-lossy).
 */
export function resetPaneAttribution(leafKey: string): Promise<void> {
  return invoke("reset_pane_attribution", { leafKey });
}

/**
 * Per-pane agent state for the dashboard (U4; running-state U3): whether the pane
 * runs a Claude Code agent, its current work stretch (ms; null when idle/not an
 * agent), how long since its last above-threshold output (ms), and the count of
 * live background task groups beneath it (0 for a non-agent / gone pane).
 */
export interface PaneActivity {
  isAgent: boolean;
  workingForMs: number | null;
  lastOutputAgoMs: number | null;
  liveTaskCount: number;
}

/** Poll a pane's agent/work-stretch state (U4). */
export function paneActivity(paneId: PaneId): Promise<PaneActivity> {
  return invoke<PaneActivity>("pane_activity", { paneId });
}

/**
 * One pane's full poll-tick status (poll-batching plan U1): the batched union
 * of `paneCwd` + `paneCommand` + `paneSessionId` + `paneActivity`, each field
 * keeping its per-pane command's exact semantics (agent-only `argv` /
 * `sessionId`; null timings and zero count for a non-agent; all-absent for a
 * gone pane). Mirrors Rust `pty::PaneStatus`.
 */
export interface PaneStatus {
  paneId: PaneId;
  cwd: string | null;
  isAgent: boolean;
  argv: string[] | null;
  sessionId: string | null;
  workingForMs: number | null;
  lastOutputAgoMs: number | null;
  liveTaskCount: number;
}

/**
 * Batched per-tick pane status (poll-batching plan U1/R1): one invoke per poll
 * tick regardless of pane count, replacing the per-pane fan-out that ran 3–4
 * sync commands × N panes on the backend main thread every 1.5 s. The per-pane
 * wrappers above remain for one-off probes (spawn, on-close persist); no
 * repeating poller should call them.
 */
export function panesStatus(paneIds: PaneId[]): Promise<PaneStatus[]> {
  return invoke<PaneStatus[]>("panes_status", { paneIds });
}

/**
 * Push the assembled agent roster to the backend feed cache
 * (feat-agent-state-local-feed, U5). The backend merges it with automations and
 * re-serves over the local read-only SSE endpoint. Always safe to call — a
 * disabled feed just caches and never serves. Returns whether the roster
 * changed (bumped the stream version).
 */
export function publishAgentFeed(payload: FeedPublishPayload): Promise<boolean> {
  return invoke<boolean>("publish_agent_feed", { payload });
}

/**
 * One plan-limit gauge behind Claude Code's `/usage` (mirrors Rust
 * `UsageLimit`). `kind` is `session` / `weekly_all` / `weekly_scoped` / `overage`;
 * `percent` is 0–100; `scopeLabel` carries the model name for a per-model weekly
 * limit; `isActive` flags the window currently binding.
 */
export interface UsageLimit {
  kind: string | null;
  group: string | null;
  percent: number;
  severity: string | null;
  resetsAt: string | null;
  scopeLabel: string | null;
  isActive: boolean;
}

/**
 * The live plan-usage snapshot for the dashboard (mirrors Rust `UsageSnapshot`):
 * the gauges behind `/usage`, fetched from Claude Code's own
 * `GET /api/oauth/usage` with the stored subscription OAuth token. `plan` is the
 * subscription tier. Fetched on dashboard open only — the command rejects (a
 * one-line `Err`) when not signed in or the endpoint is unreachable.
 */
export interface UsageSnapshot {
  limits: UsageLimit[];
  plan: string | null;
}

/** Fetch the live `/usage` gauges for the dashboard usage panel. */
export function usageSnapshot(): Promise<UsageSnapshot> {
  return invoke<UsageSnapshot>("usage_snapshot");
}

/**
 * How a stored session id was captured, ranked by trust (mirrors Rust
 * `SessionSource`, fix-session-pane-attribution KTD2): `poll` is a cwd-level
 * newest-mtime guess, `hook` is pane-precise via the authenticated socket (but
 * forgeable by in-pane code), `pick` is an explicit human decision.
 */
export type SessionSource = "poll" | "hook" | "pick";

/**
 * One agent leaf's resume mapping (mirrors Rust `ResumeRecord`, U2). `argv` is
 * the captured launch command (the flag source); `sessionCwd` is the project dir
 * the resumed agent runs in (KTD-H). `sessionSource` ranks how the id was
 * captured; `divergencePending` flags a pick whose pane's live session was
 * hook-reported as different (the re-pick prompt's signal). All optional — a
 * record may hold only the hook's session fields, or only the poll's argv,
 * before both writers have run.
 */
export interface ResumeRecord {
  sessionId: string | null;
  sessionCwd: string | null;
  sessionSource: SessionSource;
  divergencePending: boolean;
  argv: string[] | null;
  isAgent: boolean;
  updatedAt: number;
}

/** Load the write-through resume store, keyed by leaf key (U2/U8). */
export function loadResumeRecords(): Promise<Record<string, ResumeRecord>> {
  return invoke<Record<string, ResumeRecord>>("load_resume_records");
}

/**
 * The freshness of the session `claude --continue` would re-open in `cwd` (the
 * newest transcript in its project dir): its last real-turn time in Unix ms, or
 * null when the transcript has no timestamped turn — and the whole result is null
 * when no target exists (fix-resume-session-selection U3). The restore-time
 * stale-guard compares `lastTurnMs` against the pane's own activity to reject
 * resurrecting a session older than the pane's life (KTD-C).
 */
export interface ContinueTarget {
  lastTurnMs: number | null;
}
export function continueTarget(cwd: string): Promise<ContinueTarget | null> {
  return invoke<ContinueTarget | null>("continue_target", { cwd });
}

/**
 * How many real-turn-qualified transcripts `cwd`'s project dir holds
 * (fix-session-pane-attribution U9). The resume offer marks a poll/unset-source
 * leaf in a cwd counting >1 as higher-risk — its resume could re-attach a
 * sibling's session (R13/AE5). Count-based, not freshness-based: at crash
 * resume nothing is fresh.
 */
export function qualifyingSessionCount(cwd: string): Promise<number> {
  return invoke<number>("qualifying_session_count", { cwd });
}

/**
 * The directory a `--resume <sessionId>` must spawn in for Claude to find the
 * session. A resume record's `sessionCwd` is the hook's *live* cwd, which
 * drifts when the agent `cd`s away from its launch dir — but Claude scopes
 * `--resume` to the launch dir's project folder, so replaying in the drifted
 * cwd fails with "No conversation found". Verify-then-relocate against the
 * transcript store; null when the transcript can't be located (the caller
 * keeps the recorded cwd).
 */
export function resolveResumeSpawnCwd(
  sessionId: string,
  recordedCwd: string,
): Promise<string | null> {
  return invoke<string | null>("resolve_resume_spawn_cwd", {
    sessionId,
    recordedCwd,
  });
}

/**
 * A qualified previous session for handoff (mirrors Rust `HandoffTarget`,
 * session-handoff U1). `transcriptPath` is the backend-derived transcript file
 * the stock prompt names; `sessionCwd` is the resume record's cwd — context
 * only, since the spawn dir is pinned to the pane's live cwd
 * (fix-session-pane-attribution KTD8). `lastTurnMs` is the last real turn's
 * Unix ms — always present, since a session only qualifies with at least one
 * real conversation turn (R5). `sessionSource`/`divergencePending` carry the
 * record's trust rank and re-pick signal (fix-attribution U6, KTD2/KTD4).
 */
export interface HandoffTarget {
  sessionId: string;
  transcriptPath: string;
  sessionCwd: string | null;
  lastTurnMs: number;
  sessionSource: SessionSource;
  divergencePending: boolean;
}

/**
 * Resolve a leaf's previous session into a spawnable handoff target, or null
 * when nothing qualifies — no resume record, no transcript file, or no real
 * conversation turn (feeds the R6 notice). Resolved at chord time from the
 * durable resume record, so it works whether the old instance is still running
 * or exited hours ago (R4). `liveCwd` is the pane's current cwd, used only as
 * the transcript-derivation fallback when the record carries no cwd.
 */
export function resolveHandoffTarget(
  leafKey: string,
  liveCwd: string | null,
): Promise<HandoffTarget | null> {
  return invoke<HandoffTarget | null>("resolve_handoff_target", {
    leafKey,
    liveCwd,
  });
}

/**
 * One pick-list row (mirrors Rust `HandoffCandidate`, fix-session-pane-
 * attribution U5): a spawnable target plus the display-only snippet of its most
 * recent text-bearing turn. Selecting a candidate hands it off exactly as if it
 * had been precisely captured (R8).
 */
export interface HandoffCandidate extends HandoffTarget {
  snippet: string | null;
}

/**
 * The cwd's qualifying sessions for the pick-list (U5; R6/R7/R11), last
 * activity first, aged-out targets included so a reset stays non-lossy (KTD7).
 * Empty when nothing qualifies — the caller shows the existing "no previous
 * session" notice, never an empty picker (R11).
 */
export function listHandoffCandidates(
  leafKey: string,
  liveCwd: string | null,
): Promise<HandoffCandidate[]> {
  return invoke<HandoffCandidate[]>("list_handoff_candidates", {
    leafKey,
    liveCwd,
  });
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
