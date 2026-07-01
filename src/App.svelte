<script lang="ts">
  import { onMount } from "svelte";
  import Terminal from "./lib/Terminal.svelte";
  import ControlBar from "./lib/ControlBar.svelte";
  import Sidebar from "./lib/Sidebar.svelte";
  import HotkeyMenu from "./lib/HotkeyMenu.svelte";
  import CommandPalette from "./lib/CommandPalette.svelte";
  import NotificationPanel from "./lib/NotificationPanel.svelte";
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
    setRatio,
    collectKeys,
    ensureKeyCounterAbove,
    type Node,
    type Orientation,
    type DividerRect,
  } from "./lib/layout";
  import {
    tabDisplayTitle,
    findTab,
    closeTabIn,
    deleteWorkspaceFrom,
    flattenRaised,
    sortByAttentionPriority,
    unreadCountForLeaves,
    sourceLeafForNewTab,
    reorderWorkspaces,
    type Tab,
    type Workspace,
  } from "./lib/workspaces";
  import { buildHomeModel, effectiveAttention, effectiveTaskCount } from "./lib/home";
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
    paneCommand,
    paneSessionId,
    paneActivity,
    usageSnapshot,
    saveResumeRecord,
    saveResumeSession,
    loadResumeRecords,
    continueTarget,
    pruneResumeRecords,
    getLaunchMode,
    frontendLog,
    type PaneId,
    type PaneActivity,
    type UsageSnapshot,
    type AttentionState,
    type AttentionReason,
  } from "./ipc";
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
  import { Keymap, type KeymapActions } from "./lib/keymap";
  import { actionCommands, navCommands, type PaletteCommand } from "./lib/palette";
  import { getConfig } from "./lib/config";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    saveSession,
    loadSession,
    type SavedTab,
    type SavedPane,
    type SavedWorkspace,
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
  // leaf key → how it re-attached (fix-003 U3/U4): "precise" (--resume <id>) or
  // "imprecise" (--continue, most-recent-in-folder). Only resumed leaves appear;
  // drives the tier transparency so a degraded resume is never passed off as exact.
  let resumeTierByLeaf = $state<Record<string, ResumeTier>>({});
  let saveScrollbackEnabled = $state(false);
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
  let paletteOpen = $state(false);
  let notificationPanelOpen = $state(false);
  // Global do-not-disturb. Seeded from the config default on restore; the
  // runtime toggle is the session's source of truth, mirrored to the backend.
  let muted = $state(false);
  // Per-workspace mute (runtime only in v1), mirrored to the backend.
  let mutedWorkspaces = $state<Set<string>>(new Set());
  let sidebarCollapsed = $state(false);
  // Inline-rename target, owned here so the leader `r` chord and a sidebar
  // double-click drive the exact same edit (U16).
  let editing = $state<{ kind: "tab" | "ws"; id: string } | null>(null);
  // Initialised to the config default so the menu never shows an empty leader
  // if it is somehow opened before restore() resolves (R6).
  let leaderKey = $state("ctrl+a");
  // Idle delay (ms) before the attention-triage nudge appears once the focused
  // agent stops needing you (R16). Seeded from config on restore.
  let nudgeIdleMs = $state(1500);
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
  // Episode generation + in-flight guard for the focused-pane poll: an async
  // paneActivity() from a prior focus episode must not write back after the
  // episode was reset (it would corrupt the new pane's transition history), and
  // same-episode polls must not overlap and reorder their writes.
  let nudgeGen = 0;
  let nudgeTickBusy = false;
  // Focused-pane nudge poll cadence — tighter than the 1.5s dashboard poll since
  // it polls one pane and gates the nudge's responsiveness.
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
  const allPanes = $derived(
    workspaces.flatMap((w) =>
      w.tabs.flatMap((t) => leaves(t.tree).map((l) => ({ tabId: t.id, key: l.key }))),
    ),
  );
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
  // per workspace so a collapsed workspace still surfaces a raised agent.
  const unreadCounts = $derived(unreadByLeaf(notifications));
  const sidebarWorkspaces = $derived(
    workspaces.map((w) => {
      const tabs = w.tabs.map((t) => {
        const keys = leaves(t.tree).map((l) => l.key);
        return {
          id: t.id,
          title: tabDisplayTitle(t, cwdByLeaf),
          attention: keys.some((k) => attentionByLeaf[k] === "raised"),
          unread: unreadCountForLeaves(keys, unreadCounts),
        };
      });
      return {
        id: w.id,
        name: w.name,
        attention: tabs.some((t) => t.attention),
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

  async function split(orientation: Orientation) {
    if (!activeTab) return;
    const rect = rects.get(activeTab.focusedLeafKey);
    if (rect && !canSplit(rect, orientation)) return; // min-size clamp (R7)
    // Inherit the focused pane's cwd (U4), exactly like newTab: query fresh so a
    // just-issued `cd` is honored, falling back to the polled cache then $HOME
    // (null). Capture the source synchronously before the await (paneIdByLeaf is
    // non-$state, read directly).
    const srcTabId = activeTab.id;
    const srcKey = activeTab.focusedLeafKey;
    const srcPid = paneIdByLeaf[srcKey];
    const cwd =
      (srcPid != null ? await paneCwd(srcPid) : null) ?? (cwdByLeaf[srcKey] ?? null);
    // The active tab/focus may have changed during the await — bail unless the
    // source is still the focused leaf of the active tab, so we never split a
    // stale tree (the user can simply re-issue the split).
    if (activeTab?.id !== srcTabId || activeTab.focusedLeafKey !== srcKey) return;
    const res = splitLeaf(activeTab.tree, srcKey, orientation);
    if (!res) return;
    // Seed the new leaf's cwd in the same synchronous block that updates the tree,
    // so it's present before the Terminal mounts (Terminal reads cwd once at mount).
    cwdByLeaf = { ...cwdByLeaf, [res.added.key]: cwd };
    setActiveTree(res.tree, res.added.key);
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
  function focusDir(dir: "left" | "right" | "up" | "down") {
    if (!activeTab) return;
    const n = neighbor(rects, activeTab.focusedLeafKey, dir);
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
    // tier. Same comparator the dashboard Enter and the nudge Tab use.
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
  function startEdit(kind: "tab" | "ws", id: string) {
    editing = { kind, id };
  }
  function commitEdit(name: string) {
    const target = editing;
    editing = null;
    if (!target) return;
    const trimmed = name.trim();
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
    editing = null;
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
  } | null>(null);
  let resolveResumeOffer: ((accept: boolean) => void) | null = null;
  // Transient post-resume notice for the explicit `fly resume` path, which shows
  // no offer dialog (fix-003 U4, R5/AE3): names imprecise (`--continue`) and
  // stale-dropped panes so neither tier is hidden. Auto-dismisses; click to clear.
  // The offer path uses the in-dialog breakdown instead.
  let resumeNotice = $state<string | null>(null);
  let resumeNoticeTimer: ReturnType<typeof setTimeout> | null = null;
  function showResumeNotice(text: string | null) {
    if (text == null) return; // everything resumed exactly → nothing to disclose
    resumeNotice = text;
    if (resumeNoticeTimer) clearTimeout(resumeNoticeTimer);
    resumeNoticeTimer = setTimeout(() => (resumeNotice = null), 8000);
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
  function confirmPending() {
    const p = pendingConfirm;
    pendingConfirm = null;
    p?.onConfirm();
  }
  function cancelPending() {
    pendingConfirm = null;
  }
  // Opening the menu dismisses any pending confirm, so at most one overlay is
  // ever up — otherwise both Escape capture listeners would fire.
  function openMenu() {
    pendingConfirm = null;
    paletteOpen = false;
    if (notificationPanelOpen) setNotificationPanel(false);
    menuOpen = true;
  }
  // The command palette (a U4 follow-up): a focus-taking, type-to-run overlay.
  // Mutually exclusive with the cheat-sheet and the confirm so only one overlay
  // is ever up (their Escape handlers must not both fire).
  function openPalette() {
    pendingConfirm = null;
    menuOpen = false;
    if (notificationPanelOpen) setNotificationPanel(false);
    paletteOpen = true;
  }
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
    pendingConfirm = null;
    menuOpen = false;
    paletteOpen = false;
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
      paletteOpen ||
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
    // Register the pane's workspace so a per-workspace mute can scope to it.
    const wsId = workspaceIdForLeaf(key);
    if (wsId) void setPaneWorkspace(paneId, wsId);
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
    notifications = pruneToLeaves(notifications, allLiveLeafKeys());
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
  // Poll each live pane's cwd so an auto-named tab tracks `cd` without needing a
  // layout change to trigger a save. Cheap (/proc read per pane); only writes
  // state when something actually changed. This always-on poll is also the
  // capture point for each agent's launch argv (U4) — see captureResumeArgv.
  async function refreshCwds() {
    const entries = Object.entries(paneIdByLeaf);
    if (entries.length === 0) return;
    const results = await Promise.all(
      entries.map(async ([key, pid]) => [key, await paneCwd(pid)] as const),
    );
    const updates: Record<string, string | null> = {};
    let changed = false;
    for (const [key, cwd] of results) {
      if (cwd != null && cwd !== cwdByLeaf[key]) {
        updates[key] = cwd;
        changed = true;
      }
    }
    if (changed) cwdByLeaf = { ...cwdByLeaf, ...updates };
    void captureResumeArgv(entries);
    void captureResumeSession(entries);
  }
  // Capture each agent leaf's launch argv (the resume flag source) write-through,
  // once per leaf (U4). pane_command returns the argv only for a detected Claude
  // pane, so a bare shell upserts nothing. Best-effort: if the renderer dies the
  // poll stops, but the hook path (U3) still captures the session id, and the
  // builder's flag floor (U5) covers a missing argv.
  async function captureResumeArgv(entries: [string, PaneId][]) {
    for (const [key, pid] of entries) {
      if (resumeArgvCaptured.has(key)) continue;
      const argv = await paneCommand(pid);
      if (argv && argv.length > 0) {
        resumeArgvCaptured.add(key);
        void saveResumeRecord(key, argv);
      }
    }
  }
  // Capture each agent leaf's active session id write-through, re-capturing
  // whenever it changes (fix-003 U2, KTD-A/B). paneSessionId reads Claude's
  // transcript store, so capture is independent of the installed `fly` binary's
  // version — the skew that silently disabled the hook path — and fires before the
  // first Notification/Stop. Change-tracked (shouldCaptureSession), so the store is
  // touched only when the active session actually rotates; a null resolution
  // (non-agent, or no active transcript) never clears a captured id. The pane's cwd
  // doubles as the session cwd (the project dir resume runs in, KTD-H).
  async function captureResumeSession(entries: [string, PaneId][]) {
    // Read every pane's session id in parallel (each is an IPC round-trip), like
    // refreshCwds does for cwds, so poll latency doesn't scale with pane count.
    const results = await Promise.all(
      entries.map(async ([key, pid]) => [key, await paneSessionId(pid)] as const),
    );
    for (const [key, id] of results) {
      if (id == null) continue; // not an agent / no active transcript
      if (!shouldCaptureSession(resumeSessionByLeaf.get(key) ?? null, id)) continue;
      resumeSessionByLeaf.set(key, id);
      void saveResumeSession(key, id, cwdByLeaf[key] ?? null);
    }
  }

  // ---- agent dashboard (U7) -------------------------------------------------
  // Rise-debounce window for the background-task count (running-state KTD5): a raw
  // count must persist this long (~2 polls at the 1.5s cadence) before it surfaces
  // as `running`, so transient turn-start/tool spawns and pid-reuse blips don't
  // flash. The fall is immediate. Tunable (plan Open Question).
  const TASK_DEBOUNCE_MS = 3000;
  // Poll each live pane's agent state. Gated to while the home view is open (the
  // $effect below) so a toggle-only surface adds no always-on IPC. Rebuilt each
  // poll from the live panes, so an exited pane drops out (its process is no
  // longer `claude` → isAgent false).
  async function refreshAgents() {
    const entries = Object.entries(paneIdByLeaf);
    const results = await Promise.all(
      entries.map(async ([key, pid]) => [key, await paneActivity(pid)] as const),
    );
    const next: Record<string, PaneActivity> = {};
    for (const [key, a] of results) next[key] = a;
    const now = Date.now();
    // Rise/fall bookkeeping for the debounce (KTD5): stamp the rise on a 0 → >0
    // transition (riseAt currently null), clear it the instant the count hits 0.
    for (const [key, a] of results) {
      if (a.liveTaskCount > 0) {
        if (taskRiseAt[key] == null) taskRiseAt[key] = now;
      } else {
        taskRiseAt[key] = null;
      }
    }
    agentByLeaf = next;
    agentsPolledAt = now;
  }
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
  // Run the agent poll only while the dashboard is open (KTD-C): an immediate
  // fetch so it isn't blank, then every 1.5s; the cleanup tears down the timer.
  $effect(() => {
    if (!homeViewOpen) return;
    void refreshAgents();
    const timer = setInterval(() => void refreshAgents(), 1500);
    return () => clearInterval(timer);
  });
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

  // ---- attention-triage nudge trigger (U5) ---------------------------------
  // Watch the focused pane while the dashboard is closed and decide whether the
  // "move along" nudge should show. Re-runs (resetting the episode) on focus or
  // dashboard change; its own interval ticks the time-based trigger, so it never
  // piggybacks the homeModel $derived (KTD6). The became-busy edge comes from the
  // pane_activity poll (the attention stream has no output transition, KTD1); the
  // finished/question signal comes from reasonByLeaf.
  $effect(() => {
    const open = homeViewOpen;
    const leaf = activeTab?.focusedLeafKey ?? null;
    // New focus context → bump the generation and reset the episode (non-reactive
    // bookkeeping). A still-pending tick from a prior episode captured an older
    // gen and bails after its await, so it can't write into this episode.
    const gen = ++nudgeGen;
    nudgeActive = false;
    nudgeEngaged = false;
    nudgeMovedOn = false;
    nudgeSuppressed = false;
    nudgePrevWorking = null;
    nudgeSawRaise = false;
    nudgeTickBusy = false;
    if (open || !leaf) return; // no nudge while the dashboard is the view (R11)
    const tick = async () => {
      if (nudgeTickBusy) return; // don't overlap polls within an episode
      const pid = paneIdByLeaf[leaf];
      if (pid == null) return; // pane not spawned yet — try again next tick
      nudgeTickBusy = true;
      let a: PaneActivity;
      try {
        a = await paneActivity(pid);
      } catch {
        nudgeTickBusy = false;
        return; // a transient poll failure must not wedge the trigger
      }
      nudgeTickBusy = false;
      if (gen !== nudgeGen) return; // a newer focus episode superseded this tick
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
    void tick();
    const timer = setInterval(() => void tick(), NUDGE_POLL_MS);
    return () => clearInterval(timer);
  });

  // ---- persistence (U12) ---------------------------------------------------
  async function persist() {
    const savedWorkspaces: SavedWorkspace[] = [];
    for (const w of workspaces) {
      const savedTabs: SavedTab[] = [];
      for (const t of w.tabs) {
        const panes: Record<string, SavedPane> = {};
        for (const l of leaves(t.tree)) {
          const pid = paneIdByLeaf[l.key];
          const cwd = pid != null ? await paneCwd(pid) : (cwdByLeaf[l.key] ?? null);
          panes[l.key] = { cwd, title: null };
        }
        savedTabs.push({ tree: t.tree, panes, title: t.title });
      }
      savedWorkspaces.push({
        name: w.name,
        tabs: savedTabs,
        activeTabIndex: Math.max(0, w.tabs.findIndex((t) => t.id === w.activeTabId)),
      });
    }
    await saveSession({
      workspaces: savedWorkspaces,
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
    if (notificationPanelOpen) setNotificationPanel(false);
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
    cycleAttention,
    jumpNewestUnread,
    openNotifications,
    toggleMute,
    openMenu,
    openPalette,
    toggleSidebar: () => (sidebarCollapsed = !sidebarCollapsed),
    toggleHome,
    newWorkspace,
    closeWorkspace: () => requestDeleteWorkspace(activeWorkspaceId),
    prevWorkspace: () => shiftWorkspace(-1),
    nextWorkspace: () => shiftWorkspace(1),
    renameTab: startRenameActiveTab,
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

    // Explicit `fly resume` resumes immediately; a detected crash offers first.
    const summary = resumeTierSummary(tierByLeaf);
    let resuming = mode === "resume";
    if (mode === "offer") {
      if (!hasResumable) return empty; // all stale → bare shells, no offer to show
      resuming = await new Promise<boolean>((res) => {
        resolveResumeOffer = res;
        resumeOffer = { count: Object.keys(commands).length, tiers: summary, staleDropped };
      });
    }
    if (!resuming) return empty;

    // Explicit `fly resume` shows no dialog, so surface the imprecise/stale tiers
    // as a transient notice (R5/AE3); the offer path discloses them in-dialog.
    if (mode === "resume") showResumeNotice(resumeNoticeText(summary, staleDropped));

    // KTD-H: resume each agent in its captured session cwd when we have one.
    for (const key of Object.keys(commands)) {
      const cwd = records[key]?.sessionCwd;
      if (cwd) cwds[key] = cwd;
    }
    return { commands, tierByLeaf };
  }

  async function restore() {
    const cfg = await getConfig();
    saveScrollbackEnabled = cfg.saveScrollback;
    leaderKey = cfg.leaderKey;
    nudgeIdleMs = cfg.nudgeIdleMs;
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
      for (const w of saved.workspaces)
        for (const st of w.tabs)
          for (const [k, p] of Object.entries(st.panes)) cwds[k] = p.cwd;
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
        return { id: `ws-${nextWorkspaceId++}`, name: sw.name, tabs, activeTabId };
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
    const win = getCurrentWindow();
    await win.onCloseRequested(async (event) => {
      event.preventDefault();
      try {
        await persist();
      } finally {
        await win.destroy();
      }
    });
  }

  function reportForeground() {
    void setWindowForeground(document.hasFocus());
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
    const cwdTimer = setInterval(() => void refreshCwds(), 1500);
    return () => {
      window.removeEventListener("focus", reportForeground);
      window.removeEventListener("blur", reportForeground);
      window.removeEventListener("keydown", onWindowKeydown, true);
      clearInterval(cwdTimer);
      unlistenNotify?.();
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
    onToggleSidebar={() => (sidebarCollapsed = !sidebarCollapsed)}
    onSplitH={() => void split("horizontal")}
    onSplitV={() => void split("vertical")}
    onClosePane={closePane}
    onToggleMute={toggleMute}
    onOpenNotifications={openNotifications}
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
          <Terminal
            bind:this={paneRefs[p.key]}
            leafKey={p.key}
            focused={p.tabId === activeTab?.id && activeTab?.focusedLeafKey === p.key}
            cwd={cwdByLeaf[p.key] ?? null}
            command={resumeCommandByLeaf[p.key] ?? null}
            resumeTier={resumeTierByLeaf[p.key] ?? null}
            saveScrollback={saveScrollbackEnabled}
            {keymap}
            onFocusRequest={setActiveFocus}
            {onSpawned}
            {onAttention}
          />
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
        onRefresh={() => void refreshUsage()}
        onJump={jumpFromHome}
        onClose={toggleHome}
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

  {#if resumeNotice !== null}
    <!-- Explicit `fly resume` (no offer dialog) tier disclosure (fix-003 U4, R5).
         Passive + auto-dismissing; click to clear early. -->
    <button class="resume-notice" onclick={() => (resumeNotice = null)}>
      {resumeNotice}
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
  }
  .slot.hidden {
    display: none;
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
      13px/1.4 ui-monospace,
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
    font-size: 12px;
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
    font-size: 11px;
  }
  /* Transient explicit-resume tier notice (fix-003 U4): a quiet, dismissible toast
     pinned bottom-centre, styled like the passive cheat-sheet rather than a modal. */
  .resume-notice {
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
    font-size: 12px;
    cursor: pointer;
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.45);
    opacity: 0.92;
  }
  .resume-notice:hover {
    opacity: 1;
    background: #1a2740;
  }
</style>
