<script lang="ts">
  import { onMount } from "svelte";
  import Terminal from "./lib/Terminal.svelte";
  import ControlBar from "./lib/ControlBar.svelte";
  import Sidebar from "./lib/Sidebar.svelte";
  import HotkeyMenu from "./lib/HotkeyMenu.svelte";
  import CommandPalette from "./lib/CommandPalette.svelte";
  import NotificationPanel from "./lib/NotificationPanel.svelte";
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
    unreadCountForLeaves,
    type Tab,
    type Workspace,
  } from "./lib/workspaces";
  import {
    setWindowForeground,
    setVisiblePanes,
    setPaneWorkspace,
    setPanelOpen,
    setMuted,
    setWorkspaceMuted,
    onNotificationAdded,
    paneCwd,
    type PaneId,
    type AttentionState,
    type AttentionReason,
  } from "./ipc";
  import {
    addNotification,
    markRead,
    markReadForLeaves,
    markAllRead,
    clear as clearNotifications,
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
  // leaf key → pane component handle, so the palette can return focus to the
  // active terminal when it closes. The palette takes DOM focus; the cheat-sheet
  // (KTD3) does not, so only the palette needs this.
  let paneRefs: Record<string, { focus: () => void } | undefined> = {};
  let cwdByLeaf = $state<Record<string, string | null>>({});
  let saveScrollbackEnabled = $state(false);
  let keymap = $state<Keymap | null>(null);
  let menuOpen = $state(false);
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
  // tab visibility (U17).
  const visibleLeafKeys = $derived(
    activeTab ? leaves(activeTab.tree).map((l) => l.key) : [],
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

  function split(orientation: Orientation) {
    if (!activeTab) return;
    const rect = rects.get(activeTab.focusedLeafKey);
    if (rect && !canSplit(rect, orientation)) return; // min-size clamp (R7)
    const res = splitLeaf(activeTab.tree, activeTab.focusedLeafKey, orientation);
    if (res) setActiveTree(res.tree, res.added.key);
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
    markActiveTabRead();
  }
  function selectTab(wsId: string, tabId: string) {
    activeWorkspaceId = wsId;
    workspaces = workspaces.map((w) =>
      w.id === wsId ? { ...w, activeTabId: tabId } : w,
    );
    markActiveTabRead();
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
    markActiveTabRead();
  }
  function cycleAttention() {
    const raised = flattenRaised(workspaces, attentionByLeaf);
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
  function newTab(wsId: string = activeWorkspaceId) {
    const t = makeTab();
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
    markActiveTabRead();
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
  // Mark every notification on the active tab's leaves read — the "viewed a tab"
  // transition. Called only on explicit switches, so a restored session's
  // initial tab keeps its unread history (no auto-read on launch).
  function markActiveTabRead() {
    if (!activeTab) return;
    notifications = markReadForLeaves(
      notifications,
      leaves(activeTab.tree).map((l) => l.key),
    );
  }
  function jumpNewestUnread() {
    const n = newestUnread(notifications);
    if (!n) return;
    const loc = locateLeaf(n.leafKey);
    if (loc) focusPane(loc.wsId, loc.tabId, n.leafKey); // also marks the tab read
    notifications = markRead(notifications, [n.id]); // best-effort if pane is gone
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
    notifications = markRead(notifications, [id]);
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

  // App-wide leader handling (R6). Each xterm already runs the keymap to gate the
  // PTY (Terminal.svelte), but that only fires when a pane holds DOM focus — so
  // after clicking a sidebar/control-bar button the leader would go dead. This
  // capture-phase window listener covers that gap, but ONLY when focus is outside
  // a terminal: if a pane is focused, xterm handles the key, and acting here too
  // would double-dispatch and cancel the chord (the keymap is one shared, stateful
  // instance). We also bail while a rename field or overlay is up so they keep
  // their own keys.
  function onWindowKeydown(e: KeyboardEvent) {
    if (
      !keymap ||
      editing ||
      menuOpen ||
      pendingConfirm ||
      paletteOpen ||
      notificationPanelOpen
    )
      return;
    if (document.activeElement?.closest(".xterm")) return; // xterm will handle it
    if (keymap.handle(e)) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  function onAttention(key: string, state: AttentionState, _reason: AttentionReason | null) {
    attentionByLeaf = { ...attentionByLeaf, [key]: state };
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
  // Poll each live pane's cwd so an auto-named tab tracks `cd` without needing a
  // layout change to trigger a save. Cheap (/proc read per pane); only writes
  // state when something actually changed.
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
  }

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
  const keymapActions: KeymapActions = {
    newTab: () => newTab(),
    splitHorizontal: () => split("horizontal"),
    splitVertical: () => split("vertical"),
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
    newWorkspace,
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

  async function restore() {
    const cfg = await getConfig();
    saveScrollbackEnabled = cfg.saveScrollback;
    leaderKey = cfg.leaderKey;
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
      // Restore is NOT auto-read: a notification missed last session stays
      // unread on the tab you reopen into (markReadForLeaves only fires on an
      // explicit tab switch / panel open, U21).
      notifications = saved.notifications;
    } else {
      const ws = makeWorkspace("default");
      workspaces = [ws];
      activeWorkspaceId = ws.id;
    }
    ready = true;

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
    void restore();
    // Own the notification history listener here (attention arrives prop-drilled
    // from Terminal; this is a direct App listener). Resolve paneId → leafKey at
    // ingestion via the reverse index, so the entry stores the stable key.
    let unlistenNotify: (() => void) | undefined;
    void onNotificationAdded((ev) => {
      const leafKey = leafByPaneId[ev.paneId];
      if (!leafKey) return; // unknown pane (already gone) — best effort
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
    onSplitH={() => split("horizontal")}
    onSplitV={() => split("vertical")}
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
        onNewTab={(wsId) => newTab(wsId)}
        onSelectWorkspace={selectWorkspace}
        onNewWorkspace={newWorkspace}
        onDeleteWorkspace={requestDeleteWorkspace}
        onStartEdit={startEdit}
        onCommitEdit={commitEdit}
        onCancelEdit={cancelEdit}
        onToggleWorkspaceMute={toggleWorkspaceMute}
        onToggleCollapsed={() => (sidebarCollapsed = true)}
      />
    {/if}
    <div
      class="layout"
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
    </div>
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
</style>
