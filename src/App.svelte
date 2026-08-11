<script lang="ts">
  import { onMount, untrack } from "svelte";
  import Terminal from "./lib/Terminal.svelte";
  import ControlBar from "./lib/ControlBar.svelte";
  import Sidebar from "./lib/Sidebar.svelte";
  import HotkeyMenu from "./lib/HotkeyMenu.svelte";
  import CommandPalette from "./lib/CommandPalette.svelte";
  import NotificationPanel from "./lib/NotificationPanel.svelte";
  import SettingsMenu, { type ToggleSetting } from "./lib/SettingsMenu.svelte";
  import SessionPicker from "./lib/SessionPicker.svelte";
  import HomeView from "./lib/HomeView.svelte";
  import NudgeOverlay from "./lib/NudgeOverlay.svelte";
  import {
    newLeaf,
    splitLeaf,
    removeLeaf,
    leaves,
    computeRects,
    dividers,
    canSplit,
    neighbor,
    cycleLeafKey,
    setRatio,
    collectKeys,
    ensureKeyCounterAbove,
    type Node,
    type Orientation,
    type DividerRect,
  } from "./lib/layout";
  import { prunePaneIdMaps } from "./lib/pane-maps";
  import {
    tabDisplayTitle,
    findTab,
    closeTabIn,
    deleteWorkspaceFrom,
    flattenRaised,
    sortByAttentionPriority,
    rollupAttentionKind,
    combineAttentionKinds,
    unreadCountForLeaves,
    sourceLeafForNewTab,
    reorderWorkspaces,
    persistedTabs,
    scrollbackLeafKeys,
    type Tab,
    type Workspace,
  } from "./lib/workspaces";
  import {
    buildHomeModel,
    busyAgentCount,
    effectiveAttention,
    effectiveTaskCount,
  } from "./lib/home";
  import { buildFeedPayload } from "./lib/feed";
  import {
    findAutomationsWorkspace,
    buildAgentArgv,
    shouldAutoCloseRun,
    monitorCloseTarget,
  } from "./lib/automation-panes";
  import {
    automationsToRows,
    planPickup,
    sanitizeBundleText,
    type AutomationRow,
    type PickupCheckResult,
  } from "./lib/automations";
  import {
    shouldShowNudge,
    deriveBusyIdle,
    userIdleMs,
    needsYouNow,
    keyAction,
  } from "./lib/nudge";
  import {
    resumeCommandsForLeaves,
    shouldCaptureSession,
    classifyResumeTier,
    planResumeLeaves,
    resumeTierSummary,
    resumeOfferBreakdown,
    resumeNoticeText,
    isAmbiguousResumeLeaf,
    isPreciseSource,
    type ResumeTier,
  } from "./lib/resume";
  import {
    setWindowForeground,
    setVisiblePanes,
    setPaneWorkspace,
    setPanelOpen,
    setMuted,
    setWorkspaceMuted,
    onNotificationAdded,
    paneCwd,
    panesStatus,
    ptyWrite,
    publishAgentFeed,
    usageSnapshot,
    saveResumeRecord,
    saveResumeSession,
    loadResumeRecords,
    continueTarget,
    qualifyingSessionCount,
    resolveResumeSpawnCwd,
    resolveHandoffTarget,
    listHandoffCandidates,
    saveSessionPick,
    resetPaneAttribution,
    pruneResumeRecords,
    getLaunchMode,
    frontendLog,
    onAgentRun,
    automationsFrontendReady,
    listAutomations,
    deleteAutomation,
    monitorPickupCheck,
    readMonitorBundle,
    onAutomationChanged,
    onAlertPending,
    onRunClosed,
    onMonitorRegistered,
    registerAlertSink,
    type PaneId,
    type PaneActivity,
    type UsageSnapshot,
    type AttentionState,
    type AttentionReason,
    type AgentRunEvent,
    type AlertPendingEvent,
    type RunClosedEvent,
    type MonitorRegisteredEvent,
    type HandoffTarget,
    type HandoffCandidate,
  } from "./ipc";
  import {
    buildHandoffCommand,
    handoffPrompt,
    sanitizeTranscriptPath,
    type HandoffMode,
    type GuidedHandoffByLeaf,
  } from "./lib/handoff";
  import {
    candidatesToRows,
    sortCandidates,
    pickerPlan,
    takesPrecisePath,
    shouldForceSessionPick,
    provenanceLabel,
    shortSessionId,
  } from "./lib/session-picker";
  import {
    addNotification,
    markAllRead,
    clear as clearNotifications,
    clearForLeaves,
    clearAll as clearAllNotifications,
    newestUnread,
    unreadByLeaf,
    unreadTotal,
    pruneToLeaves,
    toPersisted,
    type Notification,
    type NotificationView,
  } from "./lib/notifications";
  import { Keymap, leaderLiteralBytes, type KeymapActions } from "./lib/keymap";
  import { actionCommands, navCommands, type PaletteCommand } from "./lib/palette";
  import { getConfig, setConfig } from "./lib/config";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    saveSession,
    loadSession,
    toSavedWorkspaces,
    type SavedPane,
    type SavedSession,
  } from "./lib/serialize";

  let nextTabId = 1;
  let nextWorkspaceId = 1;
  function makeTab(): Tab {
    const l = newLeaf();
    return { id: `tab-${nextTabId++}`, tree: l, focusedLeafKey: l.key, title: null };
  }
  function makeWorkspace(name: string): Workspace {
    const t = makeTab();
    return { id: `ws-${nextWorkspaceId++}`, name, tabs: [t], activeTabId: t.id };
  }

  // Automations-workspace-and-model U7 (R1/R3/KTD2): resolve — or provision —
  // the one durable "Automations" workspace that hosts every automation agent
  // run and the alerts-log tab. Returns its id. Provisions a fresh role-marked
  // workspace when none exists (so it is silently recreated after a user deletes
  // it, R3); resolution is by the persisted `role`, never the in-memory id, so a
  // run always lands in the same workspace across restarts (R2). Appends only —
  // never switches the active workspace/tab/focus (the background-tab contract).
  function ensureAutomationsWorkspace(): string {
    const existing = findAutomationsWorkspace(workspaces);
    if (existing) return existing;
    const ws: Workspace = {
      ...makeWorkspace("Automations"),
      role: "automations",
    };
    workspaces = [...workspaces, ws];
    return ws.id;
  }

  // ---- state ---------------------------------------------------------------
  let workspaces = $state<Workspace[]>([]);
  let activeWorkspaceId = $state("");
  let ready = $state(false);
  let attentionByLeaf = $state<Record<string, AttentionState>>({});
  // Last-raise reason per leaf (U5), kept current by onAttention — it carries the
  // reason even when a focused raise collapses to acknowledged, the finished-vs-
  // question discriminator the attention STATE can't express (KTD1). Cleared to
  // null when the backend's on_input takes the pane to Idle (you replied).
  let reasonByLeaf = $state<Record<string, AttentionReason | null>>({});
  // Notification history (U20). Owned here; the backend emits seed events and
  // this list carries the read/unread/cleared lifecycle (KTD16).
  let notifications = $state<Notification[]>([]);
  // leaf key → backend pane id (for cwd queries) and last-known cwd (auto names).
  let paneIdByLeaf: Record<string, PaneId> = {};
  // Peer-receive consent (agent-peer-messaging U3, R6/KTD6): leaf key → the
  // human's per-pane opt-in for `fly send` delivery. Deliberately $state
  // (drives the dashboard toggle + the publish effect) and deliberately NOT
  // persisted — seeded empty every launch so receiving always starts closed,
  // and there is no on-disk copy a same-uid process could edit. The toggle
  // below is the only writer; the bit reaches the backend solely via the
  // roster push.
  let peerOptInByLeaf = $state<Record<string, boolean>>({});
  // Reverse index pane id → leaf key, to resolve a notification event's paneId
  // to the stable leafKey it's stored under. Overwritten on each spawn, so a
  // reused paneId maps to the new leaf; a now-exited pane's stale entry lingers
  // (it helps the best-effort jump while the tab still exists).
  let leafByPaneId: Record<number, string> = {};
  // Wall-clock ms when you last *engaged* an agent leaf — viewed its raised pane
  // (→ acknowledged) or typed/cleared it (→ idle). Drives two dashboard guards:
  // the work-stretch grace (a residual stretch from before you engaged isn't
  // "working") and stale-ping suppression (a repeat idle notification with no new
  // output since isn't "waiting"). See gracedAgents / effectiveAttention.
  let lastEngagedAt: Record<string, number> = {};
  // leaf key → wall-clock ms when this leaf's raw background-task count last rose
  // 0 → >0, or null while it sits at 0 (running-state KTD5). Non-reactive, updated
  // each poll (refreshAgents); drives the rise-debounce so a transient tool/helper
  // spawn doesn't flash `running`. Mirrors lastEngagedAt; an exited pane reports a
  // 0 count, which clears its entry, so a leaf-key reuse re-arms the debounce.
  let taskRiseAt: Record<string, number | null> = {};
  // leaf key → pane component handle, so the palette can return focus to the
  // active terminal when it closes. The palette takes DOM focus; the cheat-sheet
  // (KTD3) does not, so only the palette needs this.
  let paneRefs: Record<string, { focus: () => void } | undefined> = {};
  let cwdByLeaf = $state<Record<string, string | null>>({});
  // leaf key → resume command (U8). Populated once at restore when resuming; a
  // missing entry means a bare shell (the normal case). Read once per Terminal
  // at mount, so it must be set before workspaces are assigned.
  let resumeCommandByLeaf = $state<Record<string, string[]>>({});
  // leaf key → automation agent command / run id (U8). An agent-run event
  // creates a background ephemeral tab; these are seeded before the tab is
  // appended so the Terminal reads them once at mount (like resumeCommandByLeaf).
  // The command is `["claude", "--dangerously-skip-permissions", prompt]` —
  // automation runs are unattended, so a permission prompt would just stall the
  // run until the 30-min deadline; automationRunId links the pane to the run so
  // the backend closes the run on Stop/exit and blocks recursion (R10).
  let automationCommandByLeaf = $state<Record<string, string[]>>({});
  let automationRunIdByLeaf = $state<Record<string, string>>({});
  // leaf key → command for the automations alert sink pane (U6). Like
  // automationCommandByLeaf but with NO run id — the sink pane is a plain
  // background `tail -f` of the alerts log, not an agent run (so it never links
  // to a run or enters the recursion registry). Read once at mount.
  let sinkCommandByLeaf = $state<Record<string, string[]>>({});
  // The leaf key of the current alerts sink tab, or null when none is open
  // (U6 single-flight guard). Non-reactive: read only in event handlers.
  let alertSinkLeafKey: string | null = null;
  // leaf key → command for a session-handoff pane (session-handoff U2,
  // docs/plans/2026-07-02-001-feat-session-handoff-plan.md). Like
  // sinkCommandByLeaf, a plain command map with NO run id — a handoff pane is
  // an ordinary pane: never linked to an automation run, never in the recursion
  // registry, no deadline (R11 by construction). Quick mode launches
  // bypass-permissions (it runs the prompt unattended) and carries the stock
  // pickup prompt as trailing argv (R7); guided stays default-permission and
  // omits the prompt (U3 pre-types it). Read once at mount. Monitor pickup
  // panes (monitor-handoff U7, R16) ride this same map — the identical
  // ordinary-pane shape: a `claude` argv with a prompt positional, default
  // permission mode, no automation linkage.
  let handoffCommandByLeaf = $state<Record<string, string[]>>({});
  // leaf key → resolved target for a GUIDED handoff pane (session-handoff U2,
  // the seam for U3): the injection controller reads which leaf awaits the
  // pre-typed prompt and what to type from here. Quick panes never appear.
  // Entries are released via clearGuidedHandoff when a pane's controller
  // reaches a terminal state (injected/skipped/cancelled) or the pane unmounts.
  let guidedHandoffByLeaf = $state<GuidedHandoffByLeaf>({});
  // leaf key → how it re-attached (fix-003 U3/U4): "precise" (--resume <id>) or
  // "imprecise" (--continue, most-recent-in-folder). Only resumed leaves appear;
  // drives the tier transparency so a degraded resume is never passed off as exact.
  let resumeTierByLeaf = $state<Record<string, ResumeTier>>({});
  let saveScrollbackEnabled = $state(false);
  // U5 (tmux-substrate plan): visible-but-unfocused panes render as 2 Hz DOM
  // snapshots of their hidden xterm buffers — the engine-floor relief. Seeded
  // from config.mirrorUnfocused; `false` restores live rendering everywhere.
  let mirrorUnfocused = $state(true);
  let keymap = $state<Keymap | null>(null);
  let menuOpen = $state(false);
  // Agent dashboard home view (U7): a hotkey-toggled main-content surface that
  // hides the terminal grid (panes stay mounted) while the Sidebar stays put.
  let homeViewOpen = $state(false);
  // leaf key → polled agent state (U7); rebuilt each poll while the dashboard is
  // open. `agentsPolledAt` anchors the live "working for" tick in HomeView.
  let agentByLeaf = $state<Record<string, PaneActivity>>({});
  let agentsPolledAt = $state(0);
  // Live `/usage` gauges for the dashboard's right-hand panel. Unlike the agent
  // poll, this is fetched once each time the dashboard opens (not on a timer):
  // it hits Claude Code's own `/api/oauth/usage`, so it stays well under any
  // rate limit. `usageError` carries the one-line failure reason to display.
  let usage = $state<UsageSnapshot | null>(null);
  let usageError = $state<string | null>(null);
  let usageLoading = $state(false);
  // Automations dashboard rows (U10, R25): fetched on dashboard open and
  // refetched on `automation://changed`. Rows are already sorted + humanized by
  // the pure `automationsToRows`; `automationsDegraded`/`automationsCorruptBak`
  // drive the R6 store-health warning.
  let automationRows = $state<AutomationRow[]>([]);
  let automationsDegraded = $state(false);
  let automationsCorruptBak = $state<string | null>(null);
  // Monitor pickup (monitor-handoff U7, R16/R17): the R17 fallback block the
  // matching retired-fail row expands with (null = none showing), and the
  // in-flight guard that keeps the pickup one-action (AE4) — $state so the
  // buttons disable while a pickup resolves.
  let pickupFallback = $state<{
    automationId: string;
    explanation: string;
    bundleText: string | null;
  } | null>(null);
  let pickupInFlight = $state(false);
  let paletteOpen = $state(false);
  let notificationPanelOpen = $state(false);
  // Settings menu (`leader ,`, the ⚙ control-bar button, or the command
  // palette): a focus-taking overlay, mutually exclusive with the others.
  let settingsOpen = $state(false);
  // Config-backed chrome toggle: whether the control-bar 🔔 shows. Seeded from
  // config in restore(); flipped through the settings menu, which persists it
  // via setConfig. Hiding it never disables notifications (leader n still works).
  let showNotificationsIcon = $state(true);
  // Global do-not-disturb. Seeded from the config default on restore; the
  // runtime toggle is the session's source of truth, mirrored to the backend.
  let muted = $state(false);
  // Per-workspace mute (runtime only in v1), mirrored to the backend.
  let mutedWorkspaces = $state<Set<string>>(new Set());
  let sidebarCollapsed = $state(false);
  // Inline-rename target, owned here so the leader `r` chord and a sidebar
  // double-click drive the exact same edit (U16). `pane` targets a leaf key:
  // the per-pane label editor rendered centered over the pane itself.
  let editing = $state<{ kind: "tab" | "ws" | "pane"; id: string } | null>(null);
  // Per-pane display labels, keyed by leafKey (stable across paneId
  // reassignment, like every other per-pane map). Absent = unlabeled; persisted
  // through `SavedPane.title` so labels survive restart. This is what tells
  // split siblings under one tab apart — the tab title can't.
  let paneTitleByLeaf = $state<Record<string, string>>({});
  // Initialised to the config default so the menu never shows an empty leader
  // if it is somehow opened before restore() resolves (R6).
  let leaderKey = $state("ctrl+a");
  // Idle delay (ms) before the attention-triage nudge appears once the focused
  // agent stops needing you (R16). Seeded from config on restore.
  let nudgeIdleMs = $state(1500);
  // Whether the local read-only feed is enabled (feat-agent-state-local-feed).
  // Seeded from config on restore. When on, the agent poll runs always (not just
  // while the dashboard is open) so the feed the `game` portfolio reads stays
  // live; the backend caches the pushed roster and serves it over SSE.
  let feedEnabled = $state(false);
  // Whether the "move along" nudge overlay is showing for the focused pane (U6).
  let nudgeActive = $state(false);
  // Keystroke-only idle clock (R9, KTD6): wall-clock ms of your last keydown.
  // Non-reactive — the nudge tick re-reads it each interval. Stamped at the top of
  // onWindowKeydown (before its early-returns) so typing into a pane counts; a
  // pointer listener is deliberately omitted so reading output never defers it.
  let lastUserActivityAt = Date.now();
  // Per-episode nudge bookkeeping for the focused pane (non-reactive; the focus
  // $effect resets them, the tick owns them).
  let nudgeEngaged = false; // you've typed into the focused pane this episode
  let nudgeMovedOn = false; // it resumed working or finished since you engaged
  let nudgePrevWorking: number | null = null; // last poll's workingForMs (deriveBusyIdle)
  let nudgeSuppressed = false; // Esc dismissed it this idle episode (U6)
  let nudgeSawRaise = false; // it raised for you this episode (triage, not a fresh launch)
  // Focused-pane nudge sample cadence. Synchronous and IPC-free since the
  // poll-batching plan (KTD7): each tick samples the shared 1.5s poll's
  // agentByLeaf and advances the user-idle clock, so no episode-generation or
  // in-flight guard is needed — a reset effect re-run can't race a stale
  // async write-back anymore.
  const NUDGE_POLL_MS = 1000;
  let layoutEl: HTMLDivElement;
  let layoutW = $state(1000);
  let layoutH = $state(600);

  const activeWorkspace = $derived(
    workspaces.find((w) => w.id === activeWorkspaceId),
  );
  const activeTab = $derived(
    activeWorkspace?.tabs.find((t) => t.id === activeWorkspace.activeTabId),
  );
  const rects = $derived(
    activeTab
      ? computeRects(activeTab.tree, { x: 0, y: 0, w: layoutW, h: layoutH })
      : new Map(),
  );
  const dividerList = $derived(
    activeTab ? dividers(activeTab.tree, { x: 0, y: 0, w: layoutW, h: layoutH }) : [],
  );
  // Every pane across every workspace renders once and stays mounted (hidden
  // when not the active tab). Switching workspaces or tabs must never unmount a
  // pane, or its agent would respawn — same invariant as inactive tabs (U5/KTD5).
  // Each entry also carries whether its leaf is a scrollback-save candidate
  // (U-ID U11): leaves of ephemeral tabs must never write a scrollback file —
  // the tab has no session record, so nothing could ever prune the file. The
  // eligibility is snapshotted onto the entry rather than looked up live
  // because Terminal persists scrollback in onDestroy, *after* a closing tab
  // has already left `workspaces` — a live lookup there would misclassify
  // every closing pane as ineligible.
  const allPanes = $derived.by(() => {
    const eligible = scrollbackLeafKeys(workspaces);
    return workspaces.flatMap((w) =>
      w.tabs.flatMap((t) =>
        leaves(t.tree).map((l) => ({
          tabId: t.id,
          key: l.key,
          scrollback: eligible.has(l.key),
        })),
      ),
    );
  });
  // The visible panes = the active tab's leaves in the active workspace, pushed
  // to the backend so a raise on any visible pane acknowledges in-app — the
  // attention-suppression "looking" input, generalized from keyboard focus to
  // tab visibility (U17). The dashboard (home view) covers the terminal grid, so
  // while it's open NO pane is visible — report an empty set. Otherwise a raise on
  // the pane you opened the dashboard *from* would acknowledge ("you're looking at
  // it") and never surface as "waiting" on the dashboard itself, defeating triage.
  const visibleLeafKeys = $derived(
    homeViewOpen || !activeTab ? [] : leaves(activeTab.tree).map((l) => l.key),
  );
  // View model for the sidebar: names resolved, attention rolled up per tab and
  // per workspace so a collapsed workspace still surfaces a raised agent. The
  // rollup is reason-typed (input vs done, input wins) so the dot can tell
  // "blocked on you" from "finished a turn" — see workspaces.attentionKind.
  const unreadCounts = $derived(unreadByLeaf(notifications));
  const sidebarWorkspaces = $derived(
    workspaces.map((w) => {
      const tabs = w.tabs.map((t) => {
        const keys = leaves(t.tree).map((l) => l.key);
        return {
          id: t.id,
          title: tabDisplayTitle(t, cwdByLeaf),
          attention: rollupAttentionKind(keys, attentionByLeaf, reasonByLeaf),
          unread: unreadCountForLeaves(keys, unreadCounts),
        };
      });
      return {
        id: w.id,
        name: w.name,
        attention: combineAttentionKinds(tabs.map((t) => t.attention)),
        unread: tabs.reduce((n, t) => n + t.unread, 0),
        muted: mutedWorkspaces.has(w.id),
        tabs,
      };
    }),
  );
  // Notification panel rows: newest-first, each resolved to its source label and
  // whether its pane still lives (for the jump). Reactive, so the panel and the
  // unread badges stay consistent.
  const notificationEntries = $derived<NotificationView[]>(
    [...notifications]
      .sort((a, b) => b.ts - a.ts || b.id - a.id)
      .map((n) => {
        const loc = locateLeaf(n.leafKey);
        const ws = loc ? workspaces.find((w) => w.id === loc.wsId) : undefined;
        const tab = ws?.tabs.find((t) => t.id === loc?.tabId);
        const source =
          ws && tab ? `${ws.name} / ${tabDisplayTitle(tab, cwdByLeaf)}` : "(closed)";
        return { ...n, source, jumpable: loc !== null };
      }),
  );

  // ---- tab / workspace mutation --------------------------------------------
  function updateActiveTab(fn: (tab: Tab) => Tab) {
    workspaces = workspaces.map((w) =>
      w.id === activeWorkspaceId
        ? { ...w, tabs: w.tabs.map((t) => (t.id === w.activeTabId ? fn(t) : t)) }
        : w,
    );
  }
  function setActiveTree(tree: Node, focus?: string) {
    updateActiveTab((t) => ({ ...t, tree, focusedLeafKey: focus ?? t.focusedLeafKey }));
  }
  function setActiveFocus(key: string) {
    updateActiveTab((t) => ({ ...t, focusedLeafKey: key }));
  }

  // Shared source capture for the split-alongside operations (split/handoff):
  // clamp on min-size (R7), capture the focused leaf synchronously before any
  // await (paneIdByLeaf is non-$state, read directly), then resolve its live
  // cwd — query fresh so a just-issued `cd` is honored, falling back to the
  // polled cache then $HOME (null), exactly like newTab (U4). Callers must
  // re-check `splitSourceStale` after ALL their awaits, then do seeding + tree
  // mutation in one synchronous block (Terminal reads cwd/command once at mount).
  async function captureSplitSource(orientation: Orientation) {
    if (!activeTab) return null;
    const rect = rects.get(activeTab.focusedLeafKey);
    if (rect && !canSplit(rect, orientation)) return null;
    const srcTabId = activeTab.id;
    const srcKey = activeTab.focusedLeafKey;
    const srcPid = paneIdByLeaf[srcKey];
    const liveCwd =
      (srcPid != null ? await paneCwd(srcPid) : null) ?? (cwdByLeaf[srcKey] ?? null);
    return { srcTabId, srcKey, liveCwd };
  }
  // The active tab/focus may have changed across the awaits — a stale source
  // must bail rather than mutate a stale tree (the user simply re-issues it).
  function splitSourceStale(src: { srcTabId: string; srcKey: string }): boolean {
    return activeTab?.id !== src.srcTabId || activeTab.focusedLeafKey !== src.srcKey;
  }
  async function split(orientation: Orientation) {
    const src = await captureSplitSource(orientation);
    if (!src) return;
    if (!activeTab || splitSourceStale(src)) return;
    const res = splitLeaf(activeTab.tree, src.srcKey, orientation);
    if (!res) return;
    // Seed the new leaf's cwd in the same synchronous block that updates the tree,
    // so it's present before the Terminal mounts (Terminal reads cwd once at mount).
    cwdByLeaf = { ...cwdByLeaf, [res.added.key]: src.liveCwd };
    setActiveTree(res.tree, res.added.key);
  }
  // Guard against a re-entrant handoff: the picker awaits a human, and during
  // the pre-picker async gap a second leader-f/leader-g chord from the focused
  // terminal (Terminal.svelte's key handler is overlay-unaware) would call
  // handoff() again, overwrite the first resolver, and strand its awaited
  // promise. One handoff at a time; handoffRepick() routes through here too.
  let handoffInFlight = false;
  // Session handoff (U2, docs/plans/2026-07-02-001-feat-session-handoff-plan.md;
  // ambiguity interception fix-session-pane-attribution U6): hand the focused
  // pane's previous session to a fresh agent in a split to the right (R3). Same
  // shape as split(): capture the source, await resolution (chord-time, from
  // the durable resume record — R4 via U1), staleness-bail, then seed + mutate
  // synchronously. The old pane is untouched — not closed, killed, or warned
  // (R3). When resolution is empty but candidates exist, the pick-list decides
  // (fix-attribution R6); a divergence-pending pick (a hook reported a
  // different live session, KTD2) never spawns without an explicit re-pick;
  // `forcePick` (U8) skips resolution entirely so a stale-but-resolving id can
  // still be corrected. `resetFirst` (U8 reset+re-pick) resets the leaf's
  // attribution AFTER the source is captured (below).
  async function handoff(
    mode: HandoffMode,
    opts: { forcePick?: boolean; resetFirst?: boolean } = {},
  ) {
    if (handoffInFlight) return;
    handoffInFlight = true;
    try {
      const src = await captureSplitSource("horizontal");
      if (!src) return;
      // U8 reset+re-pick (KTD7/R14): the reset lands HERE — after the source is
      // captured (so a cramped/vanished pane never destroys the stored id and
      // then bails with no picker) and against the SAME leaf captureSplitSource
      // resolved (so focus moving during the reset IPC can't split the reset and
      // the re-pick across two leaves). A cancelled re-pick still leaves no
      // stranded id, since the reset ran before the pick-list opened.
      if (opts.resetFirst) {
        try {
          await resetPaneAttribution(src.srcKey);
        } catch (e) {
          void frontendLog(`attribution reset failed: ${String(e)}`);
        }
      }
      let target: HandoffTarget | null = null;
      if (!opts.forcePick) {
        try {
          target = await resolveHandoffTarget(src.srcKey, src.liveCwd);
        } catch (e) {
          // A rejecting IPC degrades to "nothing qualifies" — the pick-list (or
          // its notice), never a blind spawn.
          void frontendLog(`handoff resolution failed: ${String(e)}`);
        }
      }
      // Route by the two tested predicates (session-picker.ts) so this glue can't
      // drift from the AE6 guarantee (R14/KTD2) or the corroborate-then-remember
      // gate (the plan's bypass-permissions Open-Question resolution): quick's
      // zero-prompt path requires an explicit Pick — an uncorroborated Hook/Poll
      // target lists once, and the remembered pick keeps later launches
      // zero-prompt. Guided stays in-loop, so any non-divergent target proceeds.
      let provenance: string | null = null;
      let pickerPathTaken = false;
      if (target && takesPrecisePath(target, mode)) {
        // The precise path (AE1): resolved with confidence — zero-prompt, but
        // the provenance is disclosed below so a remembered pick or a poll
        // guess is never invisible (KTD4).
        provenance = provenanceLabel(target.sessionSource);
      } else {
        // A divergence, an explicit force re-pick, or an uncorroborated quick
        // launch must list even a single candidate — confirming a suspect (or
        // merely unconfirmed) binding is the whole point.
        pickerPathTaken = true;
        const forceList = shouldForceSessionPick(target, opts.forcePick === true, mode);
        const subtitle = target?.divergencePending
          ? "This pane's live session differs from your remembered pick — choose again"
          : opts.forcePick
            ? "Re-pick: choose the session to bind to this pane"
            : mode === "quick"
              ? "Confirm the session to hand off — your pick is remembered for future quick launches"
              : null;
        target = await pickSession(src, { subtitle, forceList });
        if (!target) return; // cancelled, or the R11 notice already showed
      }
      if (!activeTab || splitSourceStale(src)) return;
      // KTD8/R15: pin the spawn dir to the pane's LIVE cwd. On the picker path the
      // source pane sat through a human-paced wait, so `src.liveCwd` (captured
      // pre-picker) can be stale — re-query it here so a `cd` issued while the
      // picker was open still steers the fresh agent, then re-check staleness
      // after that await (this file's await discipline). The zero-prompt precise
      // path had no wait, so its single capture stands.
      let seedCwd = src.liveCwd;
      if (pickerPathTaken) {
        const pid = paneIdByLeaf[src.srcKey];
        const fresh = pid != null ? await paneCwd(pid) : null;
        if (!activeTab || splitSourceStale(src)) return;
        seedCwd = fresh ?? src.liveCwd;
      }
      const res = splitLeaf(activeTab.tree, src.srcKey, "horizontal");
      if (!res) return;
      // Seed cwd + command in the same synchronous block that mutates the tree.
      // KTD8/R15 (supersedes handoff-plan R12): the spawn dir is PINNED to the
      // pane's live cwd — a recorded sessionCwd that diverges never steers the
      // fresh agent, quick or guided (claude auto-loads the spawn dir's project
      // config before the user reviews anything); the transcript itself still
      // rides --add-dir. R11: no automationRunId seed → the Terminal passes null
      // at spawn, so the pane never links to a run, enters the recursion gate,
      // or gets a deadline.
      cwdByLeaf = { ...cwdByLeaf, [res.added.key]: seedCwd };
      handoffCommandByLeaf = {
        ...handoffCommandByLeaf,
        [res.added.key]: buildHandoffCommand(target, mode),
      };
      // Track guided panes for U3's injection controller (which pre-types the
      // stock prompt unsent once the fresh instance's composer is ready).
      if (mode === "guided") {
        guidedHandoffByLeaf = { ...guidedHandoffByLeaf, [res.added.key]: target };
      }
      if (provenance) {
        showNotice(`Handing off ${shortSessionId(target.sessionId)}… — ${provenance}.`);
      }
      setActiveTree(res.tree, res.added.key); // focus moves to the new pane (R3)
    } finally {
      handoffInFlight = false;
    }
  }
  // Ambiguity interception (fix-attribution U6; R6-R11): fetch the cwd's
  // qualifying sessions and route by pickerPlan — none → the R11 notice (never
  // an empty picker); exactly one, unforced → zero-prompt (R9, now guided-only:
  // a quick launch always forces the list per corroborate-then-remember); else
  // await the pick-list (the resumeOffer promise pattern). An explicit selection
  // is persisted as the leaf's remembered Pick (R10) — the highest trust rank —
  // so the next launch resolves without prompting (AE2); the R9 auto path
  // deliberately mints NO pick, because no human chose.
  async function pickSession(
    src: { srcKey: string; liveCwd: string | null },
    opts: { subtitle: string | null; forceList: boolean },
  ): Promise<HandoffTarget | null> {
    let candidates: HandoffCandidate[] = [];
    let listingFailed = false;
    try {
      candidates = sortCandidates(await listHandoffCandidates(src.srcKey, src.liveCwd));
    } catch (e) {
      listingFailed = true;
      void frontendLog(`handoff candidate listing failed: ${String(e)}`);
    }
    const plan = pickerPlan(candidates.length, opts.forceList);
    if (plan === "notice") {
      // A rejected listing degrades to an empty count, which pickerPlan routes to
      // "notice" — but that is NOT the genuine R11 empty case. Distinguish them so
      // a transient backend failure doesn't misreport as "no previous session".
      showNotice(
        listingFailed
          ? "Couldn't check for a previous session — try again."
          : "No session to hand off: this pane has no previous Claude session " +
              "(no captured session, transcript missing, or no conversation turns).",
      );
      return null;
    }
    if (plan === "auto") return candidates[0];
    // One overlay at a time (the openPalette discipline) — the picker takes
    // DOM focus, and onWindowKeydown bails while it is up.
    closeAllOverlays();
    const picked = await new Promise<HandoffCandidate | null>((res) => {
      resolveSessionPicker = res;
      sessionPicker = { candidates, subtitle: opts.subtitle };
    });
    if (!picked) return null;
    // Best-effort, fire-and-forget like the poll's captures: the spawn uses
    // the picked candidate directly, so only the *memory* of the pick rides
    // this write (the next ambiguous launch would re-prompt if it fails).
    void saveSessionPick(src.srcKey, picked.sessionId, picked.sessionCwd).catch(
      (e) => frontendLog(`session pick save failed: ${String(e)}`),
    );
    return picked;
  }
  function answerSessionPicker(picked: HandoffCandidate | null) {
    sessionPicker = null;
    const r = resolveSessionPicker;
    resolveSessionPicker = null;
    r?.(picked);
    if (picked === null) focusActivePane(); // cancelled → focus back to the pane
  }
  // Reset + re-pick (fix-attribution U8, KTD7/R14): the user escape valve.
  // A thin wrapper over a forced handoff — `resetFirst` runs the reset INSIDE
  // handoff(), after captureSplitSource has resolved the source and against that
  // same leaf. That keeps the documented ordering (the reset lands before the
  // pick-list opens, so even a cancelled re-pick leaves no stranded, stale, or
  // diverged id) while no longer destroying the stored id when the handoff can't
  // proceed (a cramped or vanished pane bails before the reset), nor letting a
  // mid-flight focus change split the reset and the re-pick across two leaves.
  // The in-flight guard is shared through handoff().
  async function handoffRepick() {
    await handoff("quick", { forcePick: true, resetFirst: true });
  }
  // Session handoff U3: a guided pane's injection controller reached a terminal
  // state (injected / skipped / cancelled), or the pane unmounted first —
  // release its registry entry so the map only ever holds panes still awaiting
  // injection. Terminal fires this at most once per pane.
  function clearGuidedHandoff(key: string) {
    if (!(key in guidedHandoffByLeaf)) return;
    const { [key]: _dropped, ...rest } = guidedHandoffByLeaf;
    guidedHandoffByLeaf = rest;
  }
  function closePane() {
    if (!activeTab) return;
    const tree = removeLeaf(activeTab.tree, activeTab.focusedLeafKey);
    if (tree === null) {
      closeTab(activeTab.id);
      return;
    }
    setActiveTree(tree, leaves(tree)[0]?.key);
    pruneNotifications(); // the removed pane's leaf is gone
  }
  // Leaf-targeted close for a pane in ANY workspace/tab (vs closePane, which
  // only removes the ACTIVE tab's focused leaf). Splices the leaf out of its
  // tab's tree; when it was that tab's focused leaf, focus hands to the first
  // remaining leaf — closePane's post-removal rule — WITHOUT touching
  // activeWorkspaceId/activeTabId, so closing a background leaf never steals
  // the user's view. A sole-leaf tab is the caller's job (route through
  // closeTab); a null removeLeaf here leaves the tab untouched.
  function closeLeafInTab(tabId: string, leafKey: string) {
    workspaces = workspaces.map((w) => ({
      ...w,
      tabs: w.tabs.map((t) => {
        if (t.id !== tabId) return t;
        const tree = removeLeaf(t.tree, leafKey);
        if (tree === null) return t; // sole leaf — whole-tab close, not ours
        const focusedLeafKey =
          t.focusedLeafKey === leafKey
            ? (leaves(tree)[0]?.key ?? t.focusedLeafKey)
            : t.focusedLeafKey;
        return { ...t, tree, focusedLeafKey };
      }),
    }));
    pruneNotifications(); // the removed pane's leaf is gone
  }
  function focusDir(dir: "left" | "right" | "up" | "down") {
    if (!activeTab) return;
    const n = neighbor(rects, activeTab.focusedLeafKey, dir);
    if (n) setActiveFocus(n);
  }
  // Leader o/O: rotate focus through the active tab's panes in leaf order,
  // wrapping — geometry-free, so one repeatable chord visits every split.
  function focusCycle(delta: 1 | -1) {
    if (!activeTab) return;
    const n = cycleLeafKey(activeTab.tree, activeTab.focusedLeafKey, delta);
    if (n) setActiveFocus(n);
  }
  // Activate a specific pane anywhere (used by attention cycling, which can
  // cross workspaces): set the active workspace, its active tab, and the focus.
  function focusPane(wsId: string, tabId: string, key: string) {
    activeWorkspaceId = wsId;
    workspaces = workspaces.map((w) =>
      w.id === wsId
        ? {
            ...w,
            activeTabId: tabId,
            tabs: w.tabs.map((t) =>
              t.id === tabId ? { ...t, focusedLeafKey: key } : t,
            ),
          }
        : w,
    );
    clearActiveTabNotifications();
  }
  function selectTab(wsId: string, tabId: string) {
    activeWorkspaceId = wsId;
    workspaces = workspaces.map((w) =>
      w.id === wsId ? { ...w, activeTabId: tabId } : w,
    );
    clearActiveTabNotifications();
  }
  // Digit chord (leader 1–9 → select tab N, U1). Resolves the Nth tab (1-based)
  // in the ACTIVE workspace; out-of-range is a silent no-op. Routes through the
  // same selectTab as a click, so it clears notifications identically (U5).
  function selectTabByIndex(n: number) {
    const tab = activeWorkspace?.tabs[n - 1];
    if (tab) selectTab(activeWorkspaceId, tab.id);
  }
  function selectWorkspace(wsId: string) {
    activeWorkspaceId = wsId;
    clearActiveTabNotifications();
  }
  function cycleAttention() {
    // Payoff order (R9): question/permission before finished, positional within a
    // tier. Same comparator the dashboard Enter and the nudge Tab use. Sorts the
    // raw raised set — see nudgeRotate for the stale-re-ping residual this shares.
    const raised = sortByAttentionPriority(
      flattenRaised(workspaces, attentionByLeaf),
      reasonByLeaf,
    );
    if (raised.length === 0) return;
    const cur = raised.findIndex(
      (r) =>
        r.wsId === activeWorkspaceId &&
        r.tabId === activeTab?.id &&
        r.key === activeTab?.focusedLeafKey,
    );
    const next = raised[(cur + 1) % raised.length];
    focusPane(next.wsId, next.tabId, next.key);
  }
  async function newTab(wsId: string = activeWorkspaceId) {
    // Inherit the focused pane's cwd (U4): query fresh so a just-issued `cd` is
    // honored, falling back to the polled cache then $HOME (null). Capture the
    // source synchronously before the await so concurrent calls can't cross-wire
    // (paneIdByLeaf is non-$state, so read it directly).
    const srcKey = sourceLeafForNewTab(workspaces, wsId);
    const srcPid = srcKey != null ? paneIdByLeaf[srcKey] : null;
    const cwd =
      (srcPid != null ? await paneCwd(srcPid) : null) ??
      (srcKey != null ? (cwdByLeaf[srcKey] ?? null) : null);
    // The target workspace may have been deleted during the await — never point
    // active at a gone workspace (it would render a blank layout). Bail if so.
    if (!workspaces.some((w) => w.id === wsId)) return;
    const t = makeTab();
    const newKey = leaves(t.tree)[0].key;
    // Seed the new leaf's cwd in the same synchronous block that adds the tab, so
    // it's present before the Terminal mounts (Terminal reads cwd once at mount).
    cwdByLeaf = { ...cwdByLeaf, [newKey]: cwd };
    workspaces = workspaces.map((w) =>
      w.id === wsId ? { ...w, tabs: [...w.tabs, t], activeTabId: t.id } : w,
    );
    activeWorkspaceId = wsId;
  }
  function closeTab(tabId: string) {
    workspaces = closeTabIn(workspaces, tabId, makeTab);
    pruneNotifications(); // drop history for the closed tab's leaves
  }
  // Automations U8/R9: an agent run arrives → create a BACKGROUND ephemeral tab
  // (title = automation name) running `claude --dangerously-skip-permissions
  // <prompt>` in the run's cwd (unattended, so permissions are bypassed), placed
  // in the origin workspace (or the first, R9 fallback). It never steals focus —
  // activeWorkspaceId / the workspace's activeTabId are untouched, and allPanes
  // mounts every tab's Terminal regardless of active state, so the agent pane
  // spawns immediately in the background. The Terminal calls spawn_pane with the
  // run id, so the backend links run↔pane atomically (R10). Ephemeral so it is
  // excluded from session/scrollback persistence (U11). R12 auto-close on the
  // agent's first Stop is a deliberately deferred product decision (see the
  // plan's open questions) — the tab is kept until the user closes it.
  function handleAgentRun(ev: AgentRunEvent) {
    // U7 (R1/R5): every agent run — scheduled and "Run now" — lands in the one
    // durable Automations workspace, provisioning it if needed.
    const wsId = ensureAutomationsWorkspace();
    const l = newLeaf();
    const tab: Tab = {
      id: `tab-${nextTabId++}`,
      tree: l,
      focusedLeafKey: l.key,
      title: ev.name,
      ephemeral: true,
    };
    // Seed the leaf's cwd/command/run id BEFORE appending the tab, so they are
    // present when the Terminal mounts (all read once at mount). The launch argv
    // carries the resolved --model/--effort/--fallback-model flags (U4a → U7,
    // R11), prompt last.
    cwdByLeaf = { ...cwdByLeaf, [l.key]: ev.cwd };
    automationCommandByLeaf = {
      ...automationCommandByLeaf,
      [l.key]: buildAgentArgv(ev.prompt, {
        model: ev.model,
        effort: ev.effort,
        fallbackModel: ev.fallbackModel,
      }),
    };
    automationRunIdByLeaf = { ...automationRunIdByLeaf, [l.key]: ev.runId };
    // Append without touching activeWorkspaceId or the workspace's activeTabId —
    // background only, no focus steal.
    workspaces = workspaces.map((w) =>
      w.id === wsId ? { ...w, tabs: [...w.tabs, tab] } : w,
    );
  }
  // Automations U6/R17: an alert arrived with no sink pane → single-flight a
  // BACKGROUND ephemeral tab titled "Automations" tailing the alerts log. Like
  // handleAgentRun it never steals focus. Once the pane mounts, onSpawned calls
  // registerAlertSink → the backend drains the queued alerts and rings this pane
  // (R18). Self-healing single-flight: if the prior sink tab was closed, its
  // leaf no longer resolves, so a fresh alert opens a new one.
  function handleAlertPending(ev: AlertPendingEvent) {
    if (alertSinkLeafKey && locateLeaf(alertSinkLeafKey)) return; // already open
    // U7 (R4): the alerts-log tab lives in the Automations workspace too.
    const wsId = ensureAutomationsWorkspace();
    const l = newLeaf();
    const tab: Tab = {
      id: `tab-${nextTabId++}`,
      tree: l,
      focusedLeafKey: l.key,
      title: "Automations",
      ephemeral: true,
    };
    // Seed cwd (the log's dir) + the tail command BEFORE appending, so the
    // Terminal reads them once at mount. No run id — this is a plain pane.
    const dir = ev.logPath.replace(/\/[^/]*$/, "") || "/";
    cwdByLeaf = { ...cwdByLeaf, [l.key]: dir };
    sinkCommandByLeaf = {
      ...sinkCommandByLeaf,
      [l.key]: ["tail", "-n", "50", "-f", ev.logPath],
    };
    alertSinkLeafKey = l.key;
    workspaces = workspaces.map((w) =>
      w.id === wsId ? { ...w, tabs: [...w.tabs, tab] } : w,
    );
  }
  // Auto-close linger for a succeeded automation run's background tab (U8, R6):
  // short enough to feel immediate, long enough to glance at the result.
  const AGENT_RUN_CLOSE_LINGER_MS = 6000;
  // Automations-workspace-and-model U8 (R6/R7): an agent run closed. Map its run
  // id → leaf → enclosing tab. A succeeded run with no genuine outstanding raise
  // auto-closes after a brief linger — closeTab removes the leaf, unmounts the
  // pane, and reaps the child (KTD7). A failed run, or one still carrying a real
  // mid-run raise, keeps its tab for review (R7). A run-closed for an unknown /
  // already-gone run (e.g. the spawn never happened) is a no-op.
  function handleRunClosed(ev: RunClosedEvent) {
    const leafKey = Object.keys(automationRunIdByLeaf).find(
      (k) => automationRunIdByLeaf[k] === ev.runId,
    );
    if (!leafKey) return;
    const loc = locateLeaf(leafKey);
    if (!loc) return;
    // The backend suppresses an automation pane's normal completion raise
    // (KTD5), so a "raised" state here is a genuine mid-run signal → keep (R7).
    const raised = attentionByLeaf[leafKey] === "raised";
    if (!shouldAutoCloseRun(ev.status, raised)) return;
    const { tabId } = loc;
    setTimeout(() => closeTab(tabId), AGENT_RUN_CLOSE_LINGER_MS);
  }
  // Monitor-handoff U6 (R13): a monitor was registered from this pane — its
  // session handed the watch off to the automation, so residue-free close
  // means the REGISTERING PANE's leaf, resolved via the pure
  // monitorCloseTarget (the handleRunClosed paneId→leaf mapping pattern).
  // Only a sole-leaf tab closes whole, through the ordinary closeTab path,
  // which hands focus/activeTabId per existing close behavior; in a split tab
  // only the registering leaf is removed (closeLeafInTab) — sibling panes are
  // unrelated live sessions and survive, since killing them here would bypass
  // the destructive confirm every user-initiated multi-pane close gets
  // (requestCloseTab). NO linger, deliberately unlike handleRunClosed: the
  // event fires strictly after the store flush, so registration confirmed
  // means residue-free (origin decision). An unknown pane or an
  // already-closed tab is a no-op.
  function handleMonitorRegistered(ev: MonitorRegisteredEvent) {
    const target = monitorCloseTarget(workspaces, leafByPaneId, ev.paneId);
    if (!target) return;
    if (target.kind === "tab") {
      closeTab(target.tabId);
      return;
    }
    closeLeafInTab(target.tabId, target.leafKey);
  }
  function newWorkspace() {
    const ws = makeWorkspace(`workspace ${workspaces.length + 1}`);
    workspaces = [...workspaces, ws];
    activeWorkspaceId = ws.id;
  }
  function doDeleteWorkspace(wsId: string) {
    const res = deleteWorkspaceFrom(workspaces, wsId, () => makeWorkspace("default"));
    workspaces = res.workspaces;
    if (activeWorkspaceId === wsId) activeWorkspaceId = res.nextActiveId;
    pruneNotifications(); // drop history for the deleted workspace's leaves
    if (mutedWorkspaces.has(wsId)) {
      const next = new Set(mutedWorkspaces);
      next.delete(wsId);
      mutedWorkspaces = next;
    }
  }
  function shiftWorkspace(delta: number) {
    const idx = workspaces.findIndex((w) => w.id === activeWorkspaceId);
    if (idx === -1) return;
    const next = workspaces[(idx + delta + workspaces.length) % workspaces.length];
    activeWorkspaceId = next.id;
    clearActiveTabNotifications();
  }

  // ---- inline rename -------------------------------------------------------
  function startRenameActiveTab() {
    if (!activeTab) return;
    sidebarCollapsed = false; // reveal the field being edited
    editing = { kind: "tab", id: activeTab.id };
  }
  function startRenameFocusedPane() {
    if (!activeTab) return;
    const key = activeTab.focusedLeafKey;
    if (!key) return;
    editing = { kind: "pane", id: key };
  }
  function startEdit(kind: "tab" | "ws" | "pane", id: string) {
    editing = { kind, id };
  }
  function commitEdit(name: string) {
    const target = editing;
    editing = null;
    if (!target) return;
    const trimmed = name.trim();
    if (target.kind === "pane") {
      // Empty clears the label (back to unlabeled). The editor stole DOM focus
      // from the terminal, so hand it back either way.
      const next = { ...paneTitleByLeaf };
      if (trimmed === "") delete next[target.id];
      else next[target.id] = trimmed;
      paneTitleByLeaf = next;
      focusActivePane();
      return;
    }
    if (target.kind === "tab") {
      // Empty reverts to auto-naming (title = null).
      const title = trimmed === "" ? null : trimmed;
      workspaces = workspaces.map((w) => ({
        ...w,
        tabs: w.tabs.map((t) => (t.id === target.id ? { ...t, title } : t)),
      }));
    } else if (trimmed !== "") {
      // A workspace always needs a label, so an empty edit is a no-op.
      workspaces = workspaces.map((w) =>
        w.id === target.id ? { ...w, name: trimmed } : w,
      );
    }
  }
  function cancelEdit() {
    const wasPane = editing?.kind === "pane";
    editing = null;
    if (wasPane) focusActivePane();
  }
  // The pane-label editor's input plumbing — same shape as Sidebar.svelte's
  // rename field: Enter/Escape both unmount the input, which fires a trailing
  // blur; suppress that one blur so it can't override an explicit
  // commit/cancel.
  let suppressPaneEditBlur = false;
  function focusSelect(node: HTMLInputElement) {
    node.focus();
    node.select();
  }
  function onPaneEditKey(e: KeyboardEvent, value: string) {
    e.stopPropagation();
    if (e.key === "Enter") {
      e.preventDefault();
      suppressPaneEditBlur = true;
      commitEdit(value);
    } else if (e.key === "Escape") {
      e.preventDefault();
      suppressPaneEditBlur = true;
      cancelEdit();
    }
  }
  function onPaneEditBlur(value: string) {
    if (suppressPaneEditBlur) {
      suppressPaneEditBlur = false;
      return;
    }
    commitEdit(value);
  }

  // ---- destructive confirms (single shared overlay) ------------------------
  // leader X / the sidebar × close a tab; the sidebar × deletes a workspace.
  // Either is irreversible when it holds multiple live agent panes, so a
  // multi-pane target asks first (KTD5/R4). One overlay at a time (openMenu
  // clears it) so the two Escape capture listeners never both fire.
  let pendingConfirm = $state<{ message: string; onConfirm: () => void } | null>(
    null,
  );
  // Crash-resume offer (U8, KTD-G): shown at launch when the prior run crashed
  // and there are agents to resume. restore() awaits the answer before mounting
  // panes, so accepting spawns them as resumed agents (declining → bare shells).
  // `tiers` (fix-003 U4) breaks the count into exact (`--resume <id>`) vs
  // most-recent-in-folder (`--continue`); `staleDropped` is how many leaves were
  // dropped to a bare shell by the stale-guard — so a degraded resume is never
  // silently offered as exact, and a dropped session is disclosed (R5/AE3).
  let resumeOffer = $state<{
    count: number;
    tiers: { precise: number; imprecise: number };
    staleDropped: number;
    /** Leaves whose resume may re-attach a sibling (fix-attribution U9, R13). */
    ambiguous: number;
  } | null>(null);
  let resolveResumeOffer: ((accept: boolean) => void) | null = null;
  // Session pick-list (fix-attribution U6): the sorted candidates + context
  // line the overlay renders, and the promise handoff()/pickSession awaits —
  // the resumeOffer pattern. Null when closed; answerSessionPicker resolves.
  let sessionPicker = $state<{
    candidates: HandoffCandidate[];
    subtitle: string | null;
  } | null>(null);
  let resolveSessionPicker: ((picked: HandoffCandidate | null) => void) | null = null;
  const sessionPickerRows = $derived(
    sessionPicker ? candidatesToRows(sessionPicker.candidates, Date.now()) : [],
  );
  // Reusable transient notice toast (one surface, latest message wins). Started
  // life as the explicit-`fly resume` tier disclosure (fix-003 U4, R5/AE3 —
  // names imprecise/stale-dropped panes so neither tier is hidden; the offer
  // path uses the in-dialog breakdown instead) and was generalized for the
  // session-handoff R6 "no qualifying session" notice (U2). Auto-dismisses;
  // click to clear. A null text is a no-op (callers pass null for "nothing to
  // disclose").
  let notice = $state<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | null = null;
  function showNotice(text: string | null) {
    if (text == null) return;
    notice = text;
    if (noticeTimer) clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = null), 8000);
  }
  function answerResumeOffer(accept: boolean) {
    resumeOffer = null;
    const r = resolveResumeOffer;
    resolveResumeOffer = null;
    r?.(accept);
  }
  function paneCount(ws: Workspace): number {
    return ws.tabs.reduce((n, t) => n + leaves(t.tree).length, 0);
  }
  function requestCloseTab(tabId: string | undefined = activeTab?.id) {
    if (!tabId) return;
    const found = findTab(workspaces, tabId);
    if (!found) return;
    const panes = leaves(found.tab.tree).length;
    if (panes > 1) {
      menuOpen = false;
      paletteOpen = false;
      const name = tabDisplayTitle(found.tab, cwdByLeaf);
      pendingConfirm = {
        message: `Close tab “${name}” and its ${panes} panes?`,
        onConfirm: () => closeTab(tabId),
      };
    } else {
      closeTab(tabId);
    }
  }
  function requestDeleteWorkspace(wsId: string) {
    const ws = workspaces.find((w) => w.id === wsId);
    if (!ws) return;
    const panes = paneCount(ws);
    if (panes > 1) {
      menuOpen = false;
      paletteOpen = false;
      pendingConfirm = {
        message: `Delete workspace “${ws.name}” and its ${panes} panes?`,
        onConfirm: () => doDeleteWorkspace(wsId),
      };
    } else {
      doDeleteWorkspace(wsId);
    }
  }
  // Automations-panel delete (the dashboard row's ✕): always routes through
  // the shared destructive-confirm — a delete removes the record + its run
  // history and stops an in-flight run (R23), so unlike tab close there is no
  // "small enough to skip the ask" case. On confirm the backend emits
  // automation://changed, which refetches the panel; a failure (the automation
  // was already deleted — a raced CLI delete) surfaces as the transient notice.
  // The name is control-char-sanitized for the overlay, same as the pickup
  // notice — it is CLI-supplied text.
  function requestDeleteAutomation(row: AutomationRow) {
    menuOpen = false;
    paletteOpen = false;
    pendingConfirm = {
      message: `Delete automation “${sanitizeTranscriptPath(row.name)}” and its run history?`,
      onConfirm: () => {
        void deleteAutomation(row.id).catch((e) => {
          showNotice(`Couldn't delete the automation: ${String(e)}`);
          void frontendLog(`[fly-webview] delete_automation failed: ${String(e)}`);
        });
      },
    };
  }
  function confirmPending() {
    const p = pendingConfirm;
    pendingConfirm = null;
    p?.onConfirm();
  }
  function cancelPending() {
    pendingConfirm = null;
  }
  // At most one overlay is ever up — otherwise their Escape capture listeners
  // would double-fire. Every opener (menu, palette, notifications, the session
  // picker) closes the rest through this one helper; the opener then raises its
  // own flag. The session picker is genuinely included: a mouse-driven opener
  // (control-bar clicks aren't gated by the keydown bail list) could otherwise
  // stack over an open picker, and closing it must SETTLE its pending promise,
  // not just null the state — so route through answerSessionPicker(null) (a
  // cancel) to unwind the awaiting handoff(). pickSession() also calls this
  // before installing its own resolver, so any prior picker settles defensively.
  // answerSessionPicker(null) focuses the active pane; harmless from the openers,
  // which run closeAllOverlays() BEFORE raising their own flag and taking focus.
  function closeAllOverlays() {
    pendingConfirm = null;
    menuOpen = false;
    paletteOpen = false;
    settingsOpen = false;
    if (notificationPanelOpen) setNotificationPanel(false);
    if (sessionPicker !== null) answerSessionPicker(null);
  }
  function openMenu() {
    closeAllOverlays();
    menuOpen = true;
  }
  // The command palette (a U4 follow-up): a focus-taking, type-to-run overlay.
  function openPalette() {
    closeAllOverlays();
    paletteOpen = true;
  }
  // ---- settings menu -------------------------------------------------------
  // A focus-taking overlay like the palette; the same three openers (chord,
  // control-bar ⚙, palette command) route here. closeAllOverlays() first so it
  // supersedes any other overlay; the menu takes DOM focus, so the window keydown
  // handler bails on `settingsOpen` (below) and Esc/backdrop hand focus back.
  function openSettings() {
    closeAllOverlays();
    homeViewOpen = false;
    settingsOpen = true;
  }
  function closeSettings() {
    settingsOpen = false;
    focusActivePane();
  }
  // The settings menu is a dumb view: App owns each setting's live value and its
  // mapping to a config field. `key` round-trips back through here (opaque to the
  // menu). Optimistic — flip the runtime value, then persist; on a failed flush
  // (the backend leaves its config unchanged) revert so the toggle mirrors disk.
  async function onSettingToggle(key: string, value: boolean) {
    if (key === "showNotificationsIcon") {
      showNotificationsIcon = value;
      try {
        await setConfig({ showNotificationsIcon: value });
      } catch (e) {
        showNotificationsIcon = !value;
        void frontendLog(`settings: persist ${key} failed: ${String(e)}`);
      }
    }
  }
  // Rebuilt whenever a backing value changes so the menu's switch tracks it.
  const settingsToggles = $derived<ToggleSetting[]>([
    {
      key: "showNotificationsIcon",
      label: "Notifications icon",
      description:
        "Show the 🔔 in the control bar. Hiding it doesn't disable notifications — leader n still opens the panel.",
      value: showNotificationsIcon,
    },
  ]);
  // ---- notification panel + mute (U21) -------------------------------------
  // Replicate panel-open to the backend so KTD15's panel-open suppressor works.
  function setNotificationPanel(open: boolean) {
    notificationPanelOpen = open;
    void setPanelOpen(open);
  }
  function openNotifications() {
    // Re-invoking toggles the panel closed — reachable via the control-bar bell
    // button (onOpenNotifications). `leader n` only ever opens: while the panel
    // holds DOM focus the window keydown handler bails, so the chord can't reach
    // here. Esc / backdrop / the bell button close it (same as the palette).
    if (notificationPanelOpen) {
      closeNotifications();
      return;
    }
    closeAllOverlays();
    setNotificationPanel(true);
  }
  function closeNotifications() {
    setNotificationPanel(false);
    focusActivePane();
  }
  // Clear (remove) every notification on the active tab's leaves — the "viewed a
  // tab" transition (U5). Removing the entries clears the panel rows, the unread
  // badges, and the sidebar dot together, and the removal reaches disk. Called
  // only on explicit switches, so a restored session's initial tab keeps its
  // unread history (no auto-clear on launch). The in-pane ring is left to the
  // backend ack (Raised→Acknowledged on visibility, → Idle on first keystroke).
  function clearActiveTabNotifications() {
    if (!activeTab) return;
    notifications = clearForLeaves(
      notifications,
      leaves(activeTab.tree).map((l) => l.key),
    );
  }
  function jumpNewestUnread() {
    const n = newestUnread(notifications);
    if (!n) return;
    const loc = locateLeaf(n.leafKey);
    if (loc) focusPane(loc.wsId, loc.tabId, n.leafKey); // also clears the tab (U5)
    // Closed-pane fallback: when loc is null, focusPane/clearActiveTabNotifications
    // never ran, so clear this entry by id — else `leader U` sticks on it.
    notifications = clearNotifications(notifications, [n.id]);
  }
  function toggleMute() {
    muted = !muted;
    void setMuted(muted);
  }
  function toggleWorkspaceMute(wsId: string) {
    const next = new Set(mutedWorkspaces);
    const nowMuted = !next.has(wsId);
    if (nowMuted) next.add(wsId);
    else next.delete(wsId);
    mutedWorkspaces = next;
    void setWorkspaceMuted(wsId, nowMuted);
  }
  function onPanelJump(id: number) {
    const n = notifications.find((x) => x.id === id);
    // Remove the clicked row by id (covers the closed-pane loc===null path);
    // focusPane below also clears the rest of the destination tab's entries (U5).
    notifications = clearNotifications(notifications, [id]);
    if (n) {
      const loc = locateLeaf(n.leafKey);
      if (loc) focusPane(loc.wsId, loc.tabId, n.leafKey);
    }
    closeNotifications();
  }
  function onPanelClear(id: number) {
    notifications = clearNotifications(notifications, [id]);
  }
  function onPanelClearAll() {
    notifications = clearAllNotifications();
  }
  function onPanelMarkAllRead() {
    notifications = markAllRead(notifications);
  }
  // Hand focus back to the active terminal after the palette closes, so typing
  // and the leader keep working without a click. Deferred a frame so it lands
  // after the overlay input blurs and any tab switch from the command settles.
  function focusActivePane() {
    requestAnimationFrame(() => {
      const key = activeTab?.focusedLeafKey;
      if (key) paneRefs[key]?.focus();
    });
  }
  function closePalette() {
    paletteOpen = false;
    focusActivePane();
  }
  function runPaletteCommand(cmd: PaletteCommand) {
    paletteOpen = false; // close first, so a command that opens the confirm wins
    cmd.run();
    focusActivePane();
  }
  // Capture-phase keydown while an overlay is up, torn down with it. The
  // capture phase + stopPropagation keeps the key from reaching xterm; because
  // the listener exists only while the overlay is open, Escape reaches a
  // running TUI (vim, an agent) normally at every other time (KTD3).
  function captureKeys(handler: (e: KeyboardEvent) => void): () => void {
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }
  $effect(() => {
    if (!menuOpen) return; // hotkey menu: Escape closes
    return captureKeys((e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        menuOpen = false;
      }
    });
  });
  $effect(() => {
    if (pendingConfirm === null) return; // confirm: Enter / Escape
    return captureKeys((e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        cancelPending();
      } else if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        confirmPending();
      }
    });
  });
  $effect(() => {
    if (resumeOffer === null) return; // resume offer: Enter resumes / Escape fresh
    return captureKeys((e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        answerResumeOffer(false);
      } else if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        answerResumeOffer(true);
      }
    });
  });
  // The attention-triage nudge's permeable capture (U6, KTD2): Tab rotates, Esc
  // dismisses-and-stays (suppressing re-fire this episode), and EVERY OTHER key
  // dismisses the nudge and passes straight through to the focused xterm (R14) —
  // the one capture effect that does NOT stopPropagation on the keys it lets by,
  // so no keystroke is lost. Registered only while the nudge shows, so Tab/Esc
  // reach the PTY normally at every other time (R15).
  $effect(() => {
    if (!nudgeActive) return;
    return captureKeys((e) => {
      const action = keyAction(e.key);
      if (action === "rotate") {
        e.preventDefault(); // also suppresses browser focus traversal for Tab
        e.stopPropagation();
        nudgeRotate();
      } else if (action === "dismiss-stay") {
        e.preventDefault();
        e.stopPropagation();
        nudgeActive = false;
        nudgeSuppressed = true; // don't re-fire this idle episode (until re-raise + answer)
      } else {
        // Dismiss and let the key through to the PTY — no preventDefault /
        // stopPropagation, so the keystroke lands in the agent with nothing lost.
        nudgeActive = false;
      }
    });
  });

  // App-wide leader handling (R6). Each xterm already runs the keymap to gate the
  // PTY (Terminal.svelte), but that only fires when a pane holds DOM focus — so
  // after clicking a sidebar/control-bar button the leader would go dead. This
  // capture-phase window listener covers that gap, but ONLY when focus is outside
  // a terminal: if a pane is focused, xterm handles the key, and acting here too
  // would double-dispatch and cancel the chord (the keymap is one shared, stateful
  // instance). We also bail while a rename field or overlay is up so they keep
  // their own keys.
  function onWindowKeydown(e: KeyboardEvent) {
    // Stamp the keystroke-only idle clock first, before any early-return, so
    // typing into a focused pane still counts as activity (R9/KTD6). Typing into
    // an xterm also marks this nudge episode engaged and clears any Esc
    // suppression — a fresh keystroke is a fresh interaction.
    lastUserActivityAt = Date.now();
    if (document.activeElement?.closest(".xterm")) {
      nudgeEngaged = true;
      nudgeSuppressed = false;
    }
    if (
      !keymap ||
      editing ||
      menuOpen ||
      homeViewOpen ||
      pendingConfirm ||
      resumeOffer ||
      sessionPicker ||
      paletteOpen ||
      settingsOpen ||
      notificationPanelOpen
    )
      return;
    // NB: do NOT bail on nudgeActive here. When a pane is focused the next check
    // bails anyway (the nudge's own capture effect handles Tab/Esc/passthrough);
    // and when focus is OFF a pane, the leader must still work while the nudge
    // shows (R6) — the nudge handler stopPropagations Tab/Esc, so a leader chord
    // consumed here never double-fires.
    if (document.activeElement?.closest(".xterm")) return; // xterm will handle it
    if (keymap.handle(e)) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  function onAttention(key: string, state: AttentionState, reason: AttentionReason | null) {
    // You "engage" an agent by viewing its raised pane (→ acknowledged) or by
    // typing / clearing it (→ idle). Recorded so the dashboard can quiet a
    // residual work stretch and stale repeat idle-pings on agents you've already
    // dealt with (see gracedAgents / effectiveAttention).
    if (state === "acknowledged" || state === "idle") {
      lastEngagedAt[key] = Date.now();
    }
    attentionByLeaf = { ...attentionByLeaf, [key]: state };
    // Keep the per-leaf reason current for the nudge trigger (U5/KTD1): the
    // backend carries it on every event (including a focused acknowledged raise),
    // and nulls it on Idle (your keystroke), so this tracks "what, if anything,
    // the agent is currently asking for".
    reasonByLeaf = { ...reasonByLeaf, [key]: reason };
  }
  // Push the visible-pane ids (active tab's leaves) to the backend. Reads the
  // non-reactive paneIdByLeaf, so onSpawned must re-call it as ids arrive.
  function pushVisiblePanes() {
    const ids = visibleLeafKeys
      .map((k) => paneIdByLeaf[k])
      .filter((id): id is PaneId => id != null);
    void setVisiblePanes(ids);
  }
  // Resolve a leaf key to its owning workspace + tab (for notification jumps and
  // workspace registration). null when the leaf no longer exists (exited pane in
  // a deleted tab) — the jump then degrades to mark-read only.
  function locateLeaf(key: string): { wsId: string; tabId: string } | null {
    for (const w of workspaces)
      for (const t of w.tabs)
        if (leaves(t.tree).some((l) => l.key === key))
          return { wsId: w.id, tabId: t.id };
    return null;
  }
  function workspaceIdForLeaf(key: string): string | null {
    return locateLeaf(key)?.wsId ?? null;
  }
  function onSpawned(key: string, paneId: PaneId) {
    paneIdByLeaf[key] = paneId;
    leafByPaneId[paneId] = key; // overwrite: a reused paneId maps to the new leaf
    // paneIdByLeaf isn't $state, so the visible-set $effect won't re-fire on a
    // late-arriving paneId (async spawn) — re-push so a freshly-spawned visible
    // pane is included (else it would transiently over-banner a looked-at pane).
    pushVisiblePanes();
    // Same reason, for the feed roster: republish so the new pane's id reaches
    // the wire immediately rather than waiting on the next homeModel recompute
    // (phone-screenshot-drop U4 — a null paneId is untargetable by a drop).
    publishFeed();
    // Register the pane's workspace so a per-workspace mute can scope to it.
    const wsId = workspaceIdForLeaf(key);
    if (wsId) void setPaneWorkspace(paneId, wsId);
    // U6: the alerts sink pane just mounted → register it so queued/future
    // alerts ring it (the backend drains the pending backlog on registration).
    if (key === alertSinkLeafKey) void registerAlertSink(paneId);
  }
  // All leaf keys that still resolve to a live tab/workspace — used to prune
  // notifications whose pane was deleted (else orphaned unread counts leak).
  function allLiveLeafKeys(): Set<string> {
    const set = new Set<string>();
    for (const w of workspaces)
      for (const t of w.tabs) for (const l of leaves(t.tree)) set.add(l.key);
    return set;
  }
  function pruneNotifications() {
    const live = allLiveLeafKeys();
    notifications = pruneToLeaves(notifications, live);
    // Audit-remediation U9/KTD9: the pane-id maps must hold entries only for
    // live leaves — every close path already routes through here, so the
    // id-map prune shares the notification prune's lifecycle.
    const pruned = prunePaneIdMaps(live, paneIdByLeaf, leafByPaneId);
    paneIdByLeaf = pruned.paneIdByLeaf;
    leafByPaneId = pruned.leafByPaneId;
  }

  function startDrag(d: DividerRect, ev: PointerEvent) {
    ev.preventDefault();
    if (!activeTab) return;
    const horizontal = d.orientation === "horizontal";
    const move = (e: PointerEvent) => {
      const base = layoutEl.getBoundingClientRect();
      const ratio = horizontal
        ? (e.clientX - base.left - d.parent.x) / d.parent.w
        : (e.clientY - base.top - d.parent.y) / d.parent.h;
      setActiveTree(
        setRatio(activeTab!.tree, d.splitKey, Math.min(0.9, Math.max(0.1, ratio))),
      );
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }

  // ---- live cwd (auto tab names) -------------------------------------------
  // Leaf keys whose launch argv we've already captured into the resume store
  // (U4). A pane's launch command is fixed, so once captured it never changes —
  // recording once per leaf keeps the write-through store churn-free while still
  // catching a bash pane that later becomes a `claude` agent (uncaptured until
  // then). Non-reactive bookkeeping, like paneIdByLeaf.
  let resumeArgvCaptured = new Set<string>();
  // Last session id captured per leaf (fix-003 U2, KTD-B). Unlike argv — fixed for
  // a pane's life, captured once — a session id rotates within a pane's life
  // (`/clear`, a new conversation), so this tracks the last id seen and re-captures
  // on a change, keeping the stored id current. The in-memory guard keeps the
  // write-through store churn-free at the ~1.5s cadence. Non-reactive, like
  // resumeArgvCaptured.
  let resumeSessionByLeaf = new Map<string, string>();
  // The one per-tick poller (poll-batching plan U3, R1): a single batched
  // `panesStatus` invoke replaces the old refreshCwds/captureResumeArgv/
  // captureResumeSession/refreshAgents fan-out of 3–4 sync invokes per pane —
  // which ran as blocking work on the backend main thread and was measured
  // stalling input 200–300 ms every 1.5 s
  // (docs/notes/2026-08-08-typing-latency-diagnosis.md). Each capture keeps its
  // prior semantics exactly; only the transport is batched:
  //  - cwd: change-tracked, a null resolution never clears (auto tab names +
  //    the persistence probe's warm cache);
  //  - argv: captured once per leaf, agent-only (resume flag source, U4);
  //  - session id: change-tracked via shouldCaptureSession, null never clears
  //    (fix-003 U2, KTD-A/B — the version-skew-proof fallback capture; the
  //    backend now TTLs the underlying dir scan ~5 s, within design because
  //    the SessionStart hook remains the precise path);
  //  - activity: the dashboard/feed/nudge signal, plus the task-count
  //    rise-debounce bookkeeping (running-state KTD5).
  async function refreshPanes() {
    const entries = Object.entries(paneIdByLeaf);
    if (entries.length === 0) return;
    let statuses;
    try {
      statuses = await panesStatus(entries.map(([, pid]) => pid));
    } catch {
      return; // a transient IPC failure skips the tick, never wedges the poll
    }
    const byPane = new Map(statuses.map((s) => [s.paneId, s]));
    const now = Date.now();
    const updates: Record<string, string | null> = {};
    let cwdChanged = false;
    const nextAgents: Record<string, PaneActivity> = {};
    for (const [key, pid] of entries) {
      const s = byPane.get(pid);
      if (!s) continue;
      if (s.cwd != null && s.cwd !== cwdByLeaf[key]) {
        updates[key] = s.cwd;
        cwdChanged = true;
      }
      if (!resumeArgvCaptured.has(key) && s.argv && s.argv.length > 0) {
        resumeArgvCaptured.add(key);
        void saveResumeRecord(key, s.argv);
      }
      if (
        s.sessionId != null &&
        shouldCaptureSession(resumeSessionByLeaf.get(key) ?? null, s.sessionId)
      ) {
        resumeSessionByLeaf.set(key, s.sessionId);
        void saveResumeSession(key, s.sessionId, s.cwd ?? cwdByLeaf[key] ?? null);
      }
      nextAgents[key] = {
        isAgent: s.isAgent,
        workingForMs: s.workingForMs,
        lastOutputAgoMs: s.lastOutputAgoMs,
        liveTaskCount: s.liveTaskCount,
      };
      // Rise/fall bookkeeping for the task-count debounce (KTD5): stamp the
      // rise on a 0 → >0 transition, clear it the instant the count hits 0.
      if (s.liveTaskCount > 0) {
        if (taskRiseAt[key] == null) taskRiseAt[key] = now;
      } else {
        taskRiseAt[key] = null;
      }
    }
    if (cwdChanged) cwdByLeaf = { ...cwdByLeaf, ...updates };
    agentByLeaf = nextAgents;
    agentsPolledAt = now;
  }

  // ---- agent dashboard (U7) -------------------------------------------------
  // Rise-debounce window for the background-task count (running-state KTD5): a raw
  // count must persist this long (~2 polls at the 1.5s cadence) before it surfaces
  // as `running`, so transient turn-start/tool spawns and pid-reuse blips don't
  // flash. The fall is immediate. Tunable (plan Open Question).
  const TASK_DEBOUNCE_MS = 3000;
  // Grace: drop a lingering work stretch for a leaf whose output all predates
  // your last engagement — a residual stretch from a turn you've already seen
  // must not resurrect a "working" timer. Keeps buildHomeModel pure.
  function gracedAgents(now: number): Record<string, PaneActivity> {
    const out: Record<string, PaneActivity> = {};
    for (const [key, a] of Object.entries(agentByLeaf)) {
      const engagedAt = lastEngagedAt[key];
      if (a.workingForMs != null && engagedAt != null) {
        const lastOutputAt = now - (a.lastOutputAgoMs ?? 0);
        if (lastOutputAt <= engagedAt) {
          out[key] = { ...a, workingForMs: null };
          continue;
        }
      }
      out[key] = a;
    }
    return out;
  }
  // Rise-debounce each leaf's raw liveTaskCount (KTD5), overwriting the count on a
  // copy so buildHomeModel reads the already-effective value (mirrors gracedAgents
  // rewriting workingForMs). A count only surfaces once it has outlived the window;
  // a fall to 0 takes effect immediately (effectiveTaskCount handles both).
  function debouncedAgents(
    agents: Record<string, PaneActivity>,
    now: number,
  ): Record<string, PaneActivity> {
    const out: Record<string, PaneActivity> = {};
    for (const [key, a] of Object.entries(agents)) {
      const eff = effectiveTaskCount(a.liveTaskCount, taskRiseAt[key] ?? null, now, TASK_DEBOUNCE_MS);
      out[key] = eff === a.liveTaskCount ? a : { ...a, liveTaskCount: eff };
    }
    return out;
  }
  // Apply all three engagement/liveness guards against the same `now`, then group:
  // stale raises → idle (effectiveAttention), residual stretches → not working
  // (gracedAgents), and the raw task count → debounced (debouncedAgents) so an
  // idle pane with persistent background work reads `running`.
  let homeModel = $derived.by(() => {
    const now = Date.now();
    const agents = debouncedAgents(gracedAgents(now), now);
    const att = effectiveAttention(attentionByLeaf, agentByLeaf, lastEngagedAt, now);
    return buildHomeModel(workspaces, agents, cwdByLeaf, att, reasonByLeaf);
  });
  // A worker-driven interval that survives window backgrounding
  // (fix-feed-stale-status-while-backgrounded). WebKit throttles/pauses
  // main-thread `setInterval` when the fly window is occluded/minimized (the same
  // finding `reportForeground` documents), which freezes the agent poll — so the
  // dashboard reads stale on switch-back AND the SSE feed the `game` portfolio
  // consumes goes stale until the throttled poll resumes. A Web Worker's timer
  // runs on its own thread and is *not* subject to page-visibility throttling, so
  // ticks keep arriving at full cadence while fly is backgrounded. Classic
  // Blob-URL worker (no bundling / module / asset-protocol — that path is the
  // WebKitGTK blank-window minefield `fly-strip-crossorigin` guards). Falls back
  // to a plain `setInterval` if a worker can't be created, so the poll degrades
  // to today's behavior rather than stopping entirely.
  function startPollTicker(ms: number, onTick: () => void): () => void {
    try {
      const url = URL.createObjectURL(
        new Blob([`setInterval(()=>postMessage(0),${ms})`], {
          type: "text/javascript",
        }),
      );
      const worker = new Worker(url);
      URL.revokeObjectURL(url); // worker script is already loaded; free the URL
      worker.onmessage = () => onTick();
      return () => worker.terminate();
    } catch {
      const timer = setInterval(onTick, ms);
      return () => clearInterval(timer);
    }
  }
  // The merged poll runs always-on at 1.5s on the un-throttled worker ticker
  // (poll-batching plan U3, R7): the cwd/argv/session captures were always-on
  // before the merge (their old main-thread interval throttled while
  // backgrounded — the worker ticker strictly improves them), and the feed
  // needs a live roster whenever the app runs (feat-agent-state-local-feed
  // KTD-C). One batched invoke per tick makes the previously-gated activity
  // half effectively free, so the homeViewOpen/feedEnabled gate now only
  // controls *consumers* (feed publish, dashboard render), not the poll. The
  // ticker itself starts in onMount beside the other listeners; this effect
  // pulls one immediate refresh whenever a consumer surface opens, so the
  // dashboard reads current on open instead of one interval later.
  $effect(() => {
    if (!homeViewOpen && !feedEnabled) return;
    void refreshPanes();
  });
  // Publish the assembled roster to the backend feed cache whenever it changes
  // (U5). Reads the same `homeModel` the dashboard renders, so the feed can
  // never drift from what fly shows. The backend dedups (a no-op publish when
  // the roster is unchanged never bumps the SSE stream), so pushing on every
  // recompute is cheap. Gated on `feedEnabled` so a disabled feed adds no IPC.
  // The roster push itself, callable outside the effect. `paneIdByLeaf` isn't
  // $state, so a late-arriving paneId (async spawn) can't re-trigger the effect
  // — `onSpawned` re-pushes explicitly, exactly as it does for the visible-pane
  // set above. Without that a just-spawned agent would publish `paneId: null`
  // and stay untargetable by the phone drop until the next homeModel recompute.
  function publishFeed() {
    if (!feedEnabled) return;
    void publishAgentFeed(buildFeedPayload(homeModel, paneIdByLeaf, peerOptInByLeaf));
  }
  $effect(() => {
    // Read `homeModel` (and the peer consent map) inside the effect so both
    // stay tracked dependencies — a toggle must reach the backend on the next
    // publish, not the next roster recompute.
    void homeModel;
    void peerOptInByLeaf;
    publishFeed();
  });
  // The dashboard toggle (agent-peer-messaging U3): the one writer of the
  // consent map. Keyed by leafKey so it survives paneId reassignment.
  function togglePeerOptIn(leafKey: string) {
    peerOptInByLeaf = {
      ...peerOptInByLeaf,
      [leafKey]: !(peerOptInByLeaf[leafKey] ?? false),
    };
  }
  // Fetch the live `/usage` gauges once per dashboard open (no timer): this
  // effect re-runs only when `homeViewOpen` flips, so reopening re-fetches while
  // an open dashboard never re-polls a remote endpoint.
  async function refreshUsage() {
    usageLoading = true;
    try {
      usage = await usageSnapshot();
      usageError = null;
    } catch (e) {
      usageError = String(e);
      usage = null;
    } finally {
      usageLoading = false;
    }
  }
  $effect(() => {
    if (!homeViewOpen) return;
    void refreshUsage();
  });
  // Fetch the automations list + store health for the dashboard panel (U10).
  // Called on dashboard open and on every `automation://changed`; `nowMs` is
  // captured per fetch so the relative times ("in 5 minutes") are current.
  async function refreshAutomations() {
    try {
      const dash = await listAutomations();
      // Monitor-handoff U7: the broken-monitor inputs (derived infra-failure
      // counts + the one Rust threshold) ride the DTO — the view-model never
      // re-derives run-history walks or hardcodes the number.
      automationRows = automationsToRows(
        dash.automations,
        Date.now(),
        {
          infraFailures: dash.infraFailures,
          brokenThreshold: dash.monitorBrokenThreshold,
        },
        // Headless-agent-automations R9: the config dispatch default rides
        // the DTO so the row's effective disposition matches the claim's.
        dash.headlessDefault,
      );
      automationsDegraded = dash.degraded;
      automationsCorruptBak = dash.corruptBak;
    } catch (e) {
      void frontendLog(`[fly-webview] listAutomations failed: ${String(e)}`);
    }
  }
  $effect(() => {
    if (!homeViewOpen) return;
    void refreshAutomations();
  });
  // Monitor-handoff U7 (R16/R17): the retired-fail row's one-action pickup.
  // Validate the stored pointers still resolve (R17, over the read-only
  // monitor_pickup_check command), then either spawn ONE recovery session —
  // a normal, non-ephemeral tab in the CURRENT workspace running `claude`
  // with the stock pickup prompt in default permission mode (the user is
  // present; contrast the automations workspace + bypass flag of agent runs)
  // — or expand the row with the raw bundle + explanation instead of a broken
  // spawn. The in-flight guard + disabled buttons keep it exactly one spawn
  // per click (AE4); pickup panes are ordinary panes (no run id, no recursion
  // registry, no deadline), riding handoffCommandByLeaf like handoff panes.
  async function handleMonitorPickup(row: AutomationRow) {
    if (pickupInFlight) return;
    pickupInFlight = true;
    pickupFallback = null;
    try {
      // R17 first: check existence only when there are pointers to check —
      // planPickup routes a missing/failed check to the fallback branch.
      let check: PickupCheckResult | null = null;
      if (row.pickupPointers != null) {
        try {
          check = await monitorPickupCheck(
            row.pickupPointers.transcriptPath,
            row.pickupPointers.sessionCwd,
          );
        } catch (e) {
          void frontendLog(`monitor pickup check failed: ${String(e)}`);
        }
      }
      const plan = planPickup(row, check);
      if (plan.kind === "fallback") {
        // Best-effort bundle read (scoped + capped backend-side); a failed
        // read still shows the explanation — never a silent no-op.
        let bundleText: string | null = null;
        if (row.bundlePath != null) {
          try {
            bundleText = sanitizeBundleText(await readMonitorBundle(row.bundlePath));
          } catch (e) {
            void frontendLog(`monitor bundle read failed: ${String(e)}`);
          }
        }
        // HomeView renders the fallback block row-anchored, so if the
        // automation was deleted while we awaited (the automation://changed
        // refetch dropped its row), it would have nowhere to render — surface
        // the outcome as the transient notice instead of a silent dead click.
        if (automationRows.some((r) => r.id === row.id)) {
          pickupFallback = { automationId: row.id, explanation: plan.explanation, bundleText };
        } else {
          showNotice(
            `Monitor “${sanitizeTranscriptPath(row.name)}” was deleted while checking pickup — its failure details can't be shown.`,
          );
        }
        return;
      }
      // Spawn: seed cwd + command BEFORE appending the tab (Terminal reads
      // both once at mount), then activate it in the current workspace and
      // close the dashboard so the user lands in the recovery session.
      const t = makeTab();
      t.title = `pickup: ${sanitizeTranscriptPath(row.name)}`;
      const newKey = leaves(t.tree)[0].key;
      cwdByLeaf = { ...cwdByLeaf, [newKey]: plan.cwd };
      handoffCommandByLeaf = { ...handoffCommandByLeaf, [newKey]: plan.argv };
      workspaces = workspaces.map((w) =>
        w.id === activeWorkspaceId ? { ...w, tabs: [...w.tabs, t], activeTabId: t.id } : w,
      );
      homeViewOpen = false;
    } finally {
      pickupInFlight = false;
    }
  }

  // ---- attention-triage nudge trigger (U5) ---------------------------------
  // Watch the focused pane while the dashboard is closed and decide whether the
  // "move along" nudge should show. Re-runs (resetting the episode) on focus or
  // dashboard change; its own interval ticks the time-based trigger, so it never
  // piggybacks the homeModel $derived (KTD6). The became-busy edge samples the
  // shared merged poll's `agentByLeaf` (the attention stream has no output
  // transition, KTD1) — it used to issue its own 1 Hz `pane_activity` IPC,
  // which cost a full /proc walk per second on the backend main thread exactly
  // while the user typed (poll-batching plan KTD7); now the 1 s interval is
  // IPC-free and only advances the user-idle clock + samples the 1.5 s-fresh
  // shared data. The finished/question signal comes from reasonByLeaf.
  $effect(() => {
    const open = homeViewOpen;
    const leaf = activeTab?.focusedLeafKey ?? null;
    // New focus context → reset the episode (non-reactive bookkeeping).
    nudgeActive = false;
    nudgeEngaged = false;
    nudgeMovedOn = false;
    nudgeSuppressed = false;
    nudgePrevWorking = null;
    nudgeSawRaise = false;
    if (open || !leaf) return; // no nudge while the dashboard is the view (R11)
    const tick = () => {
      const pid = paneIdByLeaf[leaf];
      if (pid == null) return; // pane not spawned yet — try again next tick
      // The shared poll may not have covered this leaf yet (just spawned) —
      // skip rather than fabricate an idle sample.
      const a = agentByLeaf[leaf];
      if (!a) return;
      const transition = deriveBusyIdle(nudgePrevWorking, a.workingForMs);
      nudgePrevWorking = a.workingForMs;
      const att = attentionByLeaf[leaf] ?? "idle";
      const rsn = reasonByLeaf[leaf] ?? null;
      // Latch that this agent actually raised for you this episode — you're
      // triaging it, not merely launching it. Set before the needsYouNow return
      // so arriving on a question/permission raise still counts (you came to
      // handle it). Without a raise there's nothing to "move along" from (R9).
      if (att === "raised" || rsn !== null) nudgeSawRaise = true;
      if (needsYouNow(att, rsn)) {
        // The focused agent is awaiting your answer — reset and stay silent (R10).
        nudgeMovedOn = false;
        nudgeSuppressed = false;
        nudgeActive = false;
        return;
      }
      // Latch "moved on" only after you've engaged (typed), so a residual
      // pre-engagement work stretch doesn't read as a fresh resumed-working.
      if (nudgeEngaged && (transition !== "none" || rsn === "finished")) {
        nudgeMovedOn = true;
      }
      if (nudgeSuppressed) {
        nudgeActive = false;
        return;
      }
      nudgeActive = shouldShowNudge({
        engaged: nudgeEngaged,
        sawRaise: nudgeSawRaise,
        attention: att,
        reason: rsn,
        movedOn: nudgeMovedOn,
        userIdleMs: userIdleMs(Date.now(), lastUserActivityAt),
        nudgeIdleMs,
      });
    };
    // The immediate sample runs synchronously inside the effect, so its state
    // reads (agentByLeaf, attentionByLeaf, …) must be untracked — otherwise
    // every 1.5s poll assignment would re-run the effect and reset the episode.
    // Interval callbacks are untracked by nature.
    untrack(tick);
    const timer = setInterval(tick, NUDGE_POLL_MS);
    return () => clearInterval(timer);
  });

  // ---- persistence (U12) ---------------------------------------------------
  async function persist() {
    // Per-leaf snapshots for every persisted tab's leaves, resolved up front
    // (paneCwd is async) so the saved-shape projection itself stays pure.
    // Ephemeral tabs are skipped here too — their leaves need no cwd probe
    // since toSavedWorkspaces drops them from the document (U-ID U11, R12).
    const panesByLeaf: Record<string, SavedPane> = {};
    for (const w of workspaces) {
      for (const t of persistedTabs(w.tabs)) {
        for (const l of leaves(t.tree)) {
          const pid = paneIdByLeaf[l.key];
          const cwd = pid != null ? await paneCwd(pid) : (cwdByLeaf[l.key] ?? null);
          panesByLeaf[l.key] = { cwd, title: paneTitleByLeaf[l.key] ?? null };
        }
      }
    }
    await saveSession({
      // Ephemeral tabs never enter the saved document (U-ID U11, R12/KTD-G).
      workspaces: toSavedWorkspaces(workspaces, panesByLeaf),
      activeWorkspaceIndex: Math.max(
        0,
        workspaces.findIndex((w) => w.id === activeWorkspaceId),
      ),
      sidebarCollapsed,
      // Metadata-only unless saveScrollback is on (bodies can carry agent
      // output); cleared entries are already gone (KTD16 privacy).
      notifications: toPersisted(notifications, saveScrollbackEnabled),
    });
  }

  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    // Re-run on any layout/selection/sidebar change; debounce the write (R13).
    void workspaces;
    void activeWorkspaceId;
    void sidebarCollapsed;
    void notifications; // persist the history as it changes too
    void paneTitleByLeaf; // pane labels ride SavedPane.title
    if (!ready) return;
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => void persist(), 800);
  });

  // Replicate the visible-pane set whenever it changes (tab/workspace switch,
  // split, close). Late-arriving paneIds on spawn are covered by onSpawned's
  // re-push (visibleLeafKeys is reactive; paneIdByLeaf is not).
  $effect(() => {
    void visibleLeafKeys;
    if (!ready) return;
    pushVisiblePanes();
  });

  // The single action map shared by the keymap and the palette. Both render
  // from it together with BINDINGS, so a command can never drift from its chord
  // (R3/KTD1).
  // Toggle the dashboard home view (U6/U7). Mutually exclusive with the other
  // overlays; hands focus back to the active pane on close.
  function toggleHome() {
    if (homeViewOpen) {
      homeViewOpen = false;
      focusActivePane();
      return;
    }
    pendingConfirm = null;
    menuOpen = false;
    paletteOpen = false;
    settingsOpen = false;
    if (notificationPanelOpen) setNotificationPanel(false);
    // pickupFallback is deliberately NOT cleared on open (monitor-handoff U7,
    // R17): staleness is already handled where it matters — handleMonitorPickup
    // nulls it at the start of every attempt, and the block has its own dismiss
    // control — whereas a clear here would silently wipe a fallback that
    // resolved while the dashboard was closed (Escape mid-await) before the
    // user ever saw it.
    homeViewOpen = true;
  }
  // Jump from a dashboard row to its pane, closing the view (R4).
  function jumpFromHome(wsId: string, tabId: string, key: string) {
    homeViewOpen = false;
    focusPane(wsId, tabId, key);
    focusActivePane();
  }
  // Tab from the nudge rotates to the next agent needing attention, or back to the
  // dashboard when none remain (R2/R12). Uses the raw raised set via flattenRaised,
  // ordered by reason payoff (R9) — identical to cycleAttention (leader u) and robust without the dashboard's
  // fresh activity poll (effectiveAttention needs that, and it isn't running while
  // the dashboard is closed). The current pane is excluded so the last agent's Tab
  // goes home rather than rotating to itself (AE5). Focus change / opening the
  // dashboard both reset the nudge episode via its $effect.
  function nudgeRotate() {
    // Payoff order (R9): rotate to the next agent by reason priority
    // (question/permission before finished), positional within a tier.
    // Known residual: this sorts the *raw* raised set, so a stale idle re-ping on
    // an already-handled agent (which effectiveAttention would downgrade for the
    // dashboard) can be promoted to the front here. Aligning rotation membership
    // with effectiveAttention is the deferred follow-up (needs the activity poll
    // to cover non-focused panes while the dashboard is closed).
    const others = sortByAttentionPriority(
      flattenRaised(workspaces, attentionByLeaf),
      reasonByLeaf,
    ).filter(
      (r) =>
        !(
          r.wsId === activeWorkspaceId &&
          r.tabId === activeTab?.id &&
          r.key === activeTab?.focusedLeafKey
        ),
    );
    nudgeActive = false;
    if (others.length === 0) {
      homeViewOpen = true; // terminus: back to the home base
    } else {
      focusPane(others[0].wsId, others[0].tabId, others[0].key);
      focusActivePane();
    }
  }
  const keymapActions: KeymapActions = {
    newTab: () => void newTab(),
    splitHorizontal: () => void split("horizontal"),
    splitVertical: () => void split("vertical"),
    closePane,
    closeTab: () => requestCloseTab(),
    focusLeft: () => focusDir("left"),
    focusRight: () => focusDir("right"),
    focusUp: () => focusDir("up"),
    focusDown: () => focusDir("down"),
    focusNextPane: () => focusCycle(1),
    focusPrevPane: () => focusCycle(-1),
    cycleAttention,
    jumpNewestUnread,
    openNotifications,
    toggleMute,
    openMenu,
    openPalette,
    openSettings,
    toggleSidebar: () => (sidebarCollapsed = !sidebarCollapsed),
    toggleHome,
    newWorkspace,
    closeWorkspace: () => requestDeleteWorkspace(activeWorkspaceId),
    prevWorkspace: () => shiftWorkspace(-1),
    nextWorkspace: () => shiftWorkspace(1),
    renameTab: startRenameActiveTab,
    renamePane: startRenameFocusedPane,
    handoffQuick: () => void handoff("quick"),
    handoffGuided: () => void handoff("guided"),
    handoffRepick: () => void handoffRepick(),
    // U10/KTD10: double-tapped leader → one literal leader keystroke to the
    // focused pane. A leader with no terminal encoding (super+…) is a no-op.
    sendLiteralLeader: () => {
      const bytes = leaderLiteralBytes(leaderKey);
      const key = activeTab?.focusedLeafKey;
      const pid = key != null ? paneIdByLeaf[key] : undefined;
      if (bytes != null && pid != null) void ptyWrite(pid, bytes);
    },
  };
  // Palette commands: every leader action (from BINDINGS) plus live "jump to
  // workspace/tab" navigation, built from the same resolved view model the
  // sidebar uses. Reactive, so the nav list is current whenever it opens.
  const paletteCommands = $derived<PaletteCommand[]>([
    ...actionCommands(keymapActions, leaderKey),
    ...navCommands(sidebarWorkspaces, selectWorkspace, selectTab),
  ]);

  // Clock-jitter allowance for the stale-guard (fix-003 U3, KTD-C): a `--continue`
  // candidate counts as fresh if its last real turn is no more than this before the
  // pane's own captured activity. Generous enough to absorb the gap between a turn
  // and the poll that stamps the pane's record, tight enough that a day-old session
  // is still caught. Tunable (plan Open Questions: stale-guard margin).
  const RESUME_STALE_MARGIN_MS = 60_000;

  // Decide whether to resume and, if so, compute each restored leaf's command +
  // its resume tier (U8; fix-003 U3). Awaits the crash offer (KTD-G) before
  // returning, mutates `cwds` to prefer each agent's captured session cwd (KTD-H),
  // stale-guards the imprecise (`--continue`) leaves so a session older than the
  // pane's life is dropped to a bare shell (KTD-C, R4), and always prunes the store
  // to the live leaves. Returns the per-leaf command map (empty when not resuming)
  // alongside the tier map. Called before workspaces mount, so panes spawn resumed.
  async function computeResumeForRestore(
    saved: SavedSession,
    cwds: Record<string, string | null>,
    defaultArgs: string[],
  ): Promise<{
    commands: Record<string, string[]>;
    tierByLeaf: Record<string, ResumeTier>;
  }> {
    const savedKeys = saved.workspaces.flatMap((w) =>
      w.tabs.flatMap((t) => collectKeys(t.tree)),
    );
    // Keep the store bounded regardless of mode (drop closed-pane orphans).
    void pruneResumeRecords(savedKeys);
    const empty = { commands: {}, tierByLeaf: {} };

    const mode = await getLaunchMode();
    if (mode === "normal") return empty; // R1: a clean launch never auto-runs

    const records = await loadResumeRecords();
    const baseCommands = resumeCommandsForLeaves(savedKeys, records, defaultArgs);
    if (Object.keys(baseCommands).length === 0) return empty; // nothing resumable

    // Probe the `--continue` candidate's freshness for each imprecise (no-id) leaf
    // — a precise (`--resume <id>`) leaf bypasses the guard, so it needs no probe.
    // The probes run in PARALLEL (each is a Tauri round-trip + a transcript read),
    // and each is ISOLATED: a rejection degrades that leaf to a null candidate →
    // stale → bare shell, never aborting the whole restore (fix-003 review). The
    // candidate runs in the leaf's spawn cwd (`cwds[key]`, since an imprecise leaf
    // has no sessionCwd override yet).
    const impreciseLeaves = Object.keys(baseCommands).filter(
      (k) => classifyResumeTier(records[k]) === "imprecise",
    );
    const candidateLastTurnByLeaf: Record<string, number | null> = Object.fromEntries(
      await Promise.all(
        impreciseLeaves.map(async (k) => {
          try {
            const target = await continueTarget(cwds[k] ?? "");
            return [k, target?.lastTurnMs ?? null] as const;
          } catch {
            return [k, null] as const; // probe failed → treat as stale
          }
        }),
      ),
    );

    // Stale-guard + tier-classify (KTD-C/D): a precise leaf keeps (exact
    // re-attach); a fresh imprecise leaf keeps as `--continue`; a stale imprecise
    // leaf is dropped to a bare shell (R4) and counted for disclosure (R5/AE3).
    const { commands, tierByLeaf, staleDropped } = planResumeLeaves(
      baseCommands,
      records,
      candidateLastTurnByLeaf,
      RESUME_STALE_MARGIN_MS,
    );
    const hasResumable = Object.keys(commands).length > 0;
    if (!hasResumable && staleDropped === 0) return empty; // nothing happened

    // Ambiguity risk (fix-attribution U9, R13/AE5): a kept leaf whose stored id
    // is no better than the poll's guess (source poll / pre-fix unset), sitting
    // in a cwd that holds >1 qualifying transcript AT RESUME TIME, may re-attach
    // a sibling — count them for the offer/notice. Count-keyed, not freshness
    // (post-crash nothing is fresh). Probes run in parallel and are DEDUPED by
    // cwd (several splits often share one repo — the backend scans each project
    // dir once, not once per leaf); a probe failure degrades to unflagged
    // rather than blocking restore; precise (hook/pick) leaves skip the probe.
    const countByCwd = new Map<string, Promise<number>>();
    const probeCount = (cwd: string) => {
      let p = countByCwd.get(cwd);
      if (!p) {
        p = qualifyingSessionCount(cwd);
        countByCwd.set(cwd, p);
      }
      return p;
    };
    const ambiguous = (
      await Promise.all(
        Object.keys(commands).map(async (k) => {
          const rec = records[k];
          if (isPreciseSource(rec?.sessionSource)) return false;
          try {
            const n = await probeCount(rec?.sessionCwd ?? cwds[k] ?? "");
            return isAmbiguousResumeLeaf(rec, n);
          } catch {
            return false;
          }
        }),
      )
    ).filter(Boolean).length;

    // Explicit `fly resume` resumes immediately; a detected crash offers first.
    const summary = resumeTierSummary(tierByLeaf);
    let resuming = mode === "resume";
    if (mode === "offer") {
      if (!hasResumable) return empty; // all stale → bare shells, no offer to show
      resuming = await new Promise<boolean>((res) => {
        resolveResumeOffer = res;
        resumeOffer = {
          count: Object.keys(commands).length,
          tiers: summary,
          staleDropped,
          ambiguous,
        };
      });
    }
    if (!resuming) return empty;

    // Explicit `fly resume` shows no dialog, so surface the imprecise/stale/
    // ambiguous tiers as a transient notice (R5/AE3; U9); the offer path
    // discloses them in-dialog.
    if (mode === "resume") showNotice(resumeNoticeText(summary, staleDropped, ambiguous));

    // KTD-H: resume each agent in its captured session cwd when we have one.
    // The captured cwd is the hook's *live* cwd, which drifts when the agent
    // `cd`s away from its launch dir — but `claude --resume <id>` searches only
    // the launch dir's project folder, so a drifted record dies with "No
    // conversation found". For precise leaves, verify-then-relocate the cwd
    // against the transcript store; a null/failed probe keeps the recorded cwd
    // (no worse than before). Probes run in parallel and are isolated per leaf.
    await Promise.all(
      Object.keys(commands).map(async (key) => {
        const rec = records[key];
        const cwd = rec?.sessionCwd;
        if (!cwd) return;
        cwds[key] = cwd;
        if (!rec?.sessionId) return;
        try {
          const resolved = await resolveResumeSpawnCwd(rec.sessionId, cwd);
          if (resolved) cwds[key] = resolved;
        } catch {
          // probe failed → keep the recorded cwd
        }
      }),
    );
    return { commands, tierByLeaf };
  }

  async function restore() {
    const cfg = await getConfig();
    saveScrollbackEnabled = cfg.saveScrollback;
    mirrorUnfocused = cfg.mirrorUnfocused ?? true;
    leaderKey = cfg.leaderKey;
    nudgeIdleMs = cfg.nudgeIdleMs;
    feedEnabled = cfg.feed.enabled;
    showNotificationsIcon = cfg.showNotificationsIcon;
    // Seed global mute from the config default; the backend seeds the same value
    // at startup, so they start in sync (the runtime toggle keeps them so).
    muted = cfg.notificationsMutedDefault;
    keymap = new Keymap(cfg.leaderKey, keymapActions, selectTabByIndex);

    const saved = await loadSession();
    if (saved && saved.workspaces.length) {
      // Keep saved keys (so scrollback files match) and bump the counter past
      // them so new nodes never collide.
      ensureKeyCounterAbove(
        saved.workspaces.flatMap((w) => w.tabs.flatMap((t) => collectKeys(t.tree))),
      );
      const cwds: Record<string, string | null> = {};
      const titles: Record<string, string> = {};
      for (const w of saved.workspaces)
        for (const st of w.tabs)
          for (const [k, p] of Object.entries(st.panes)) {
            cwds[k] = p.cwd;
            if (p.title) titles[k] = p.title;
          }
      paneTitleByLeaf = titles;
      // Resume wiring (U8; fix-003 U3): may await the crash offer, populate resume
      // commands + their tiers, drop stale `--continue` leaves to bare shells, and
      // override spawn cwds — all before workspaces mount so panes spawn as resumed
      // agents. In normal mode this returns empty → every pane a bare shell.
      // A resume-probe failure (a rejecting IPC) must never blank the UI: fall
      // back to bare shells so the saved layout still mounts, rather than aborting
      // restore (fix-003 review). The per-leaf probe is itself guarded inside the
      // function; this is the outer belt-and-suspenders.
      let resumeResult: {
        commands: Record<string, string[]>;
        tierByLeaf: Record<string, ResumeTier>;
      };
      try {
        resumeResult = await computeResumeForRestore(saved, cwds, cfg.resumeDefaultArgs);
      } catch (e) {
        void frontendLog(`resume restore failed, falling back to bare shells: ${String(e)}`);
        resumeResult = { commands: {}, tierByLeaf: {} };
      }
      resumeCommandByLeaf = resumeResult.commands;
      resumeTierByLeaf = resumeResult.tierByLeaf;
      cwdByLeaf = cwds;
      workspaces = saved.workspaces.map((sw) => {
        const tabs = sw.tabs.map((st) => ({
          id: `tab-${nextTabId++}`,
          tree: st.tree,
          focusedLeafKey: leaves(st.tree)[0]?.key ?? "",
          title: st.title ?? null,
        }));
        if (tabs.length === 0) tabs.push(makeTab()); // never an empty workspace
        const activeTabId =
          tabs[Math.min(Math.max(0, sw.activeTabIndex), tabs.length - 1)]?.id ??
          tabs[0].id;
        // Carry the durable Automations-workspace role marker across restart
        // (U6, R2): a fresh in-memory id, but the role is what placement resolves.
        return {
          id: `ws-${nextWorkspaceId++}`,
          name: sw.name,
          tabs,
          activeTabId,
          ...(sw.role ? { role: sw.role } : {}),
        };
      });
      activeWorkspaceId =
        workspaces[
          Math.min(Math.max(0, saved.activeWorkspaceIndex), workspaces.length - 1)
        ]?.id ?? workspaces[0].id;
      sidebarCollapsed = saved.sidebarCollapsed;
      // Restore is NOT auto-cleared: a notification missed last session stays
      // unread on the tab you reopen into (clearActiveTabNotifications only fires
      // on an explicit tab/workspace switch, and born-cleared only drops live
      // raises — neither touches this restore path, U5/U21).
      notifications = saved.notifications;
    } else {
      const ws = makeWorkspace("default");
      workspaces = [ws];
      activeWorkspaceId = ws.id;
    }
    ready = true;
    // Open to the dashboard as the home base (R1/KTD5): set synchronously here,
    // right after `ready`, so the dashboard is the first painted view rather than
    // flashing the restored grid first. The crash-resume offer was already
    // awaited above (inside computeResumeForRestore), so opening underneath it
    // never clobbers it; panes stay mounted and run hidden behind the dashboard.
    homeViewOpen = true;

    // Flush a final save before the window closes, while the shells are still
    // alive so their cwds are captured (R13/R14). Then the backend reaps panes.
    // A busy agent (working a live turn, or running a live background task —
    // busyAgentCount) gets one confirm first, via the shared destructive-confirm
    // overlay, so an accidental × doesn't silently kill in-progress work.
    const win = getCurrentWindow();
    const closeNow = async () => {
      try {
        await persist();
      } finally {
        await win.destroy();
      }
    };
    await win.onCloseRequested(async (event) => {
      event.preventDefault();
      const busy = busyAgentCount(homeModel);
      if (busy > 0) {
        pendingConfirm = {
          message: `${busy} agent${busy === 1 ? " is" : "s are"} still working. Quit anyway?`,
          onConfirm: () => void closeNow(),
        };
        return;
      }
      await closeNow();
    });
  }

  function reportForeground() {
    const focused = document.hasFocus();
    void setWindowForeground(focused);
    // Focus-in status refresh (fix-dashboard-stale-status-on-focus): the worker
    // ticker keeps the merged poll alive while backgrounded, but a tick can
    // still be up to 1.5s stale at the moment focus returns — pull one poll
    // forward so the dashboard/roster read current on glance, not one interval
    // later. One batched invoke, same probe the ticker runs.
    if (focused) void refreshPanes();
  }
  onMount(() => {
    window.addEventListener("focus", reportForeground);
    window.addEventListener("blur", reportForeground);
    window.addEventListener("keydown", onWindowKeydown, true);
    reportForeground();
    // Last-resort safety net: if restore() throws before mounting anything, the
    // app must still come up usable (a default workspace) rather than a blank
    // window stuck at ready=false (fix-003 review).
    void restore().catch((e) => {
      void frontendLog(`restore failed: ${String(e)}`);
      if (!ready) {
        const ws = makeWorkspace("default");
        workspaces = [ws];
        activeWorkspaceId = ws.id;
        ready = true;
      }
    });
    // Own the notification history listener here (attention arrives prop-drilled
    // from Terminal; this is a direct App listener). Resolve paneId → leafKey at
    // ingestion via the reverse index, so the entry stores the stable key.
    let unlistenNotify: (() => void) | undefined;
    void onNotificationAdded((ev) => {
      const leafKey = leafByPaneId[ev.paneId];
      if (!leafKey) return; // unknown pane (already gone) — best effort
      // Born-cleared (U5): a raise on the tab the user is currently viewing is
      // never shown. `ev.read` is the backend's foreground-aware "user is
      // viewing this pane" bit; gate on it AND the leaf being on the active tab,
      // so a background-window raise (ev.read false) still records, and a
      // switch-away in the same tick (leaf no longer visible) still records.
      // Restore loads history via a different path, so the initial tab's unread
      // survives launch.
      if (ev.read && visibleLeafKeys.includes(leafKey)) return;
      notifications = addNotification(notifications, {
        id: ev.id,
        leafKey,
        reason: ev.reason,
        title: ev.title,
        body: ev.body,
        ts: ev.ts,
        read: ev.read,
      });
    }).then((un) => (unlistenNotify = un));
    // Automations U8/R5: register the agent-run listener, then tell the backend
    // the frontend is ready — only after the listener is live, so the first
    // dispatched run can't fire into a listener-less void. Until this call the
    // sweep defers due agent automations (script automations run regardless).
    let unlistenAgentRun: (() => void) | undefined;
    void onAgentRun(handleAgentRun).then((un) => {
      unlistenAgentRun = un;
      void automationsFrontendReady();
    });
    // Automations U10/R25: refetch the dashboard panel on every mutation, but
    // only while the dashboard is open (a closed dashboard re-fetches fresh on
    // its next open, so there is nothing to keep current).
    let unlistenAutomationChanged: (() => void) | undefined;
    void onAutomationChanged(() => {
      if (homeViewOpen) void refreshAutomations();
    }).then((un) => (unlistenAutomationChanged = un));
    // Automations U6/R17: an alert with no sink pane → open the "Automations"
    // sink tab (single-flighted in the handler).
    let unlistenAlertPending: (() => void) | undefined;
    void onAlertPending(handleAlertPending).then(
      (un) => (unlistenAlertPending = un),
    );
    // Automations-workspace-and-model U8: an agent run closed → auto-close a
    // succeeded run's background tab (or keep a failed / attention one).
    let unlistenRunClosed: (() => void) | undefined;
    void onRunClosed(handleRunClosed).then((un) => (unlistenRunClosed = un));
    // Monitor-handoff U6 (R13): a monitor registered → close the registering
    // pane's leaf immediately (whole tab only when it's the sole leaf; no
    // linger — see handleMonitorRegistered).
    let unlistenMonitorRegistered: (() => void) | undefined;
    void onMonitorRegistered(handleMonitorRegistered).then(
      (un) => (unlistenMonitorRegistered = un),
    );
    // The merged always-on poll (poll-batching U3): one batched invoke per
    // 1.5s tick on the un-throttled worker ticker (R7), replacing the old
    // separate cwd interval + gated agent poll. Immediate first tick so
    // restore doesn't wait 1.5s for cwds/statuses.
    void refreshPanes();
    const stopPanePoll = startPollTicker(1500, () => void refreshPanes());
    return () => {
      window.removeEventListener("focus", reportForeground);
      window.removeEventListener("blur", reportForeground);
      window.removeEventListener("keydown", onWindowKeydown, true);
      stopPanePoll();
      unlistenNotify?.();
      unlistenAgentRun?.();
      unlistenAutomationChanged?.();
      unlistenAlertPending?.();
      unlistenRunClosed?.();
      unlistenMonitorRegistered?.();
    };
  });
</script>

<div class="app">
  <ControlBar
    workspaceName={activeWorkspace?.name ?? ""}
    tabName={activeTab ? tabDisplayTitle(activeTab, cwdByLeaf) : ""}
    {sidebarCollapsed}
    {muted}
    unreadTotal={unreadTotal(notifications)}
    {showNotificationsIcon}
    onToggleSidebar={() => (sidebarCollapsed = !sidebarCollapsed)}
    onSplitH={() => void split("horizontal")}
    onSplitV={() => void split("vertical")}
    onClosePane={closePane}
    onToggleMute={toggleMute}
    onOpenNotifications={openNotifications}
    onOpenSettings={openSettings}
    onMenu={openMenu}
  />
  <div class="body">
    {#if !sidebarCollapsed}
      <Sidebar
        workspaces={sidebarWorkspaces}
        {activeWorkspaceId}
        activeTabId={activeWorkspace?.activeTabId ?? ""}
        {editing}
        onSelectTab={selectTab}
        onCloseTab={requestCloseTab}
        onNewTab={(wsId) => void newTab(wsId)}
        onSelectWorkspace={selectWorkspace}
        onNewWorkspace={newWorkspace}
        onDeleteWorkspace={requestDeleteWorkspace}
        onStartEdit={startEdit}
        onCommitEdit={commitEdit}
        onCancelEdit={cancelEdit}
        onToggleWorkspaceMute={toggleWorkspaceMute}
        onToggleCollapsed={() => (sidebarCollapsed = true)}
        onReorderWorkspace={(from, to) =>
          (workspaces = reorderWorkspaces(workspaces, from, to))}
      />
    {/if}
    <div
      class="layout"
      class:hidden={homeViewOpen}
      bind:this={layoutEl}
      bind:clientWidth={layoutW}
      bind:clientHeight={layoutH}
    >
      {#each allPanes as p (p.key)}
        {@const r = p.tabId === activeTab?.id ? rects.get(p.key) : undefined}
        <div
          class="slot"
          class:hidden={!r}
          style={r ? `left:${r.x}px;top:${r.y}px;width:${r.w}px;height:${r.h}px` : ""}
        >
          <!-- Per-pane label, centered above the pane (leader R). The tab title
               can't tell split siblings apart; this can. It sits in a reserved
               strip *above* the pane's top border rather than overlaying the
               terminal's first row, so it never covers agent output; the strip
               exists only while a label does (or is being edited), so unlabeled
               panes keep their full height. Slot rects are untouched — the
               terminal just refits to the remaining height. -->
          {#if editing?.kind === "pane" && editing.id === p.key}
            <input
              class="pane-title-edit"
              value={paneTitleByLeaf[p.key] ?? ""}
              placeholder="pane name (empty clears)"
              use:focusSelect
              onkeydown={(e) => onPaneEditKey(e, e.currentTarget.value)}
              onblur={(e) => onPaneEditBlur(e.currentTarget.value)}
            />
          {:else if paneTitleByLeaf[p.key]}
            <button
              type="button"
              class="pane-title"
              title="double-click to rename (leader R)"
              ondblclick={() => startEdit("pane", p.key)}
            >{paneTitleByLeaf[p.key]}</button>
          {/if}
          <div class="pane-host">
            <Terminal
              bind:this={paneRefs[p.key]}
              leafKey={p.key}
              focused={p.tabId === activeTab?.id &&
                activeTab?.focusedLeafKey === p.key}
              visible={p.tabId === activeTab?.id}
              mirrored={mirrorUnfocused &&
                p.tabId === activeTab?.id &&
                activeTab?.focusedLeafKey !== p.key}
              cwd={cwdByLeaf[p.key] ?? null}
              command={resumeCommandByLeaf[p.key] ??
                automationCommandByLeaf[p.key] ??
                sinkCommandByLeaf[p.key] ??
                handoffCommandByLeaf[p.key] ??
                null}
              automationRunId={automationRunIdByLeaf[p.key] ?? null}
              resumeTier={resumeTierByLeaf[p.key] ?? null}
              injectText={guidedHandoffByLeaf[p.key]
                ? handoffPrompt(guidedHandoffByLeaf[p.key].transcriptPath)
                : null}
              saveScrollback={saveScrollbackEnabled && p.scrollback}
              {keymap}
              onFocusRequest={setActiveFocus}
              {onSpawned}
              {onAttention}
              onInjectionDone={clearGuidedHandoff}
            />
          </div>
        </div>
      {/each}
      {#each dividerList as d (d.splitKey)}
        <div
          class="divider {d.orientation}"
          role="separator"
          tabindex="-1"
          aria-label="resize panes"
          style="left:{d.rect.x}px;top:{d.rect.y}px;width:{d.rect.w}px;height:{d.rect.h}px"
          onpointerdown={(e) => startDrag(d, e)}
        ></div>
      {/each}
      {#if nudgeActive}
        <NudgeOverlay
          rect={activeTab ? (rects.get(activeTab.focusedLeafKey) ?? null) : null}
        />
      {/if}
    </div>
    {#if homeViewOpen}
      <HomeView
        model={homeModel}
        polledAt={agentsPolledAt}
        usage={usage}
        usageError={usageError}
        usageLoading={usageLoading}
        automations={automationRows}
        automationsDegraded={automationsDegraded}
        automationsCorruptBak={automationsCorruptBak}
        pickupFallback={pickupFallback}
        pickupBusy={pickupInFlight}
        onPickup={(row) => void handleMonitorPickup(row)}
        onDeleteAutomation={requestDeleteAutomation}
        onDismissPickupFallback={() => (pickupFallback = null)}
        onRefresh={() => void refreshUsage()}
        onJump={jumpFromHome}
        onClose={toggleHome}
        peerOptInByLeaf={peerOptInByLeaf}
        onTogglePeer={togglePeerOptIn}
      />
    {/if}
  </div>

  {#if pendingConfirm !== null}
    <div class="backdrop" role="presentation" onpointerdown={cancelPending}>
      <div
        class="confirm"
        role="alertdialog"
        aria-label="Confirmation"
        tabindex="-1"
        onpointerdown={(e) => e.stopPropagation()}
      >
        <p class="confirm-msg">{pendingConfirm.message}</p>
        <div class="confirm-actions">
          <button class="btn danger" onclick={confirmPending}>Confirm</button>
          <button class="btn" onclick={cancelPending}>Cancel</button>
        </div>
        <p class="confirm-hint">Enter to confirm · Esc to cancel</p>
      </div>
    </div>
  {/if}

  {#if resumeOffer !== null}
    {@const breakdown = resumeOfferBreakdown(
      resumeOffer.tiers,
      resumeOffer.staleDropped,
      resumeOffer.ambiguous,
    )}
    <div class="backdrop" role="presentation">
      <div
        class="confirm"
        role="alertdialog"
        aria-label="Resume agents"
        tabindex="-1"
      >
        <p class="confirm-msg">
          fly didn't shut down cleanly. Resume {resumeOffer.count} Claude agent{resumeOffer.count ===
          1
            ? ""
            : "s"} from your last session?
        </p>
        {#if breakdown}
          <!-- Tier breakdown (fix-003 U4, R5/AE3): disclose how many re-attach
               exactly vs by most-recent-session-in-folder, and how many stale
               sessions were dropped to a fresh shell — so the degraded path is
               never passed off as exact. Null (hidden) when every pane is exact. -->
          <p class="confirm-sub">{breakdown}</p>
        {/if}
        <div class="confirm-actions">
          <button class="btn danger" onclick={() => answerResumeOffer(true)}>
            Resume
          </button>
          <button class="btn" onclick={() => answerResumeOffer(false)}>
            Start fresh
          </button>
        </div>
        <p class="confirm-hint">Enter to resume · Esc to start fresh</p>
      </div>
    </div>
  {/if}

  {#if notice !== null}
    <!-- Shared transient notice: explicit-resume tier disclosure (fix-003 U4,
         R5) and the handoff no-qualifying-session notice (session-handoff U2,
         R6). Passive + auto-dismissing; click to clear early. -->
    <button class="notice" onclick={() => (notice = null)}>
      {notice}
    </button>
  {/if}

  <HotkeyMenu
    open={menuOpen}
    leader={leaderKey}
    onClose={() => (menuOpen = false)}
  />

  <CommandPalette
    open={paletteOpen}
    commands={paletteCommands}
    onRun={runPaletteCommand}
    onClose={closePalette}
  />

  <NotificationPanel
    open={notificationPanelOpen}
    entries={notificationEntries}
    onJump={onPanelJump}
    onClear={onPanelClear}
    onClearAll={onPanelClearAll}
    onMarkAllRead={onPanelMarkAllRead}
    onClose={closeNotifications}
  />

  <SettingsMenu
    open={settingsOpen}
    toggles={settingsToggles}
    onToggle={onSettingToggle}
    onClose={closeSettings}
  />

  <SessionPicker
    open={sessionPicker !== null}
    rows={sessionPickerRows}
    subtitle={sessionPicker?.subtitle ?? null}
    onPick={(i) => answerSessionPicker(sessionPicker?.candidates[i] ?? null)}
    onClose={() => answerSessionPicker(null)}
  />
</div>

<style>
  .app {
    display: flex;
    flex-direction: column;
    width: 100vw;
    height: 100vh;
    overflow: hidden;
  }
  .body {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .layout {
    position: relative;
    flex: 1;
    min-width: 0;
    min-height: 0;
    background: #161b2c;
  }
  /* Hide the terminal grid for the home view (U7). Panes stay mounted — the
     never-unmount invariant (KTD5) — so toggling never respawns an agent. */
  .layout.hidden {
    display: none;
  }
  .slot {
    position: absolute;
    display: flex;
    flex-direction: column;
  }
  .slot.hidden {
    display: none;
  }
  /* The terminal takes whatever height the label strip (if any) leaves. */
  .pane-host {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  /* Per-pane label pill + its inline editor, centered in a reserved strip
     above the pane's top border, so neither ever covers terminal output. */
  .pane-title,
  .pane-title-edit {
    flex: none;
    align-self: center;
    max-width: 60%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font:
      15px "JetBrains Mono",
      monospace;
    border-radius: 6px 6px 0 0;
    padding: 1px 10px 2px;
  }
  .pane-title {
    color: #9fb2d8;
    background: rgba(22, 27, 44, 0.92);
    border: 1px solid #2b3a55;
    border-bottom: none;
    cursor: default;
  }
  .pane-title-edit {
    color: #dfe7f5;
    background: #0b1020;
    border: 1px solid #4a6296;
    border-bottom: none;
    outline: none;
    width: 275px;
    text-align: center;
  }
  .divider {
    position: absolute;
    z-index: 5;
    background: #161b2c;
  }
  .divider:hover {
    background: #2b3a55;
  }
  .divider.horizontal {
    cursor: col-resize;
  }
  .divider.vertical {
    cursor: row-resize;
  }
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(5, 8, 16, 0.6);
  }
  .confirm {
    background: #0b1020;
    border: 1px solid #2b3a55;
    border-radius: 6px;
    padding: 18px 20px;
    min-width: 260px;
    text-align: center;
    color: #c9d1d9;
    font:
      16px/1.4 ui-monospace,
      monospace;
    box-shadow: 0 8px 30px rgba(0, 0, 0, 0.5);
  }
  .confirm-msg {
    margin: 0 0 14px;
  }
  /* Resume-offer tier breakdown (fix-003 U4): a quiet sub-line under the prompt. */
  .confirm-sub {
    margin: -8px 0 14px;
    opacity: 0.6;
    font-size: 15px;
  }
  .confirm-actions {
    display: flex;
    gap: 8px;
    justify-content: center;
  }
  .btn {
    background: #11182b;
    border: 1px solid #2b3a55;
    color: #c9d1d9;
    border-radius: 4px;
    padding: 6px 14px;
    cursor: pointer;
    font: inherit;
  }
  .btn:hover {
    background: #1a2740;
  }
  .btn.danger {
    border-color: #f5a623;
    color: #f5a623;
  }
  .btn.danger:hover {
    background: #2a2410;
  }
  .confirm-hint {
    margin: 12px 0 0;
    opacity: 0.5;
    font-size: 14px;
  }
  /* Shared transient notice (fix-003 U4; session-handoff U2/R6): a quiet,
     dismissible toast pinned bottom-centre, styled like the passive cheat-sheet
     rather than a modal. */
  .notice {
    position: fixed;
    bottom: 16px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 50;
    max-width: 80vw;
    background: #11182b;
    border: 1px solid #2b3a55;
    color: #c9d1d9;
    border-radius: 6px;
    padding: 8px 14px;
    font: inherit;
    font-size: 15px;
    cursor: pointer;
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.45);
    opacity: 0.92;
  }
  .notice:hover {
    opacity: 1;
    background: #1a2740;
  }
</style>
