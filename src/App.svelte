<script lang="ts">
  import { onMount } from "svelte";
  import Terminal from "./lib/Terminal.svelte";
  import TabBar from "./lib/TabBar.svelte";
  import HotkeyMenu from "./lib/HotkeyMenu.svelte";
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
    setWindowForeground,
    paneCwd,
    type PaneId,
    type AttentionState,
    type AttentionReason,
  } from "./ipc";
  import { Keymap } from "./lib/keymap";
  import { getConfig } from "./lib/config";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    saveSession,
    loadSession,
    type SavedTab,
    type SavedPane,
  } from "./lib/serialize";

  interface Tab {
    id: string;
    tree: Node;
    focusedLeafKey: string;
  }

  let nextTabId = 1;
  function makeTab(): Tab {
    const l = newLeaf();
    return { id: `tab-${nextTabId++}`, tree: l, focusedLeafKey: l.key };
  }

  let tabs = $state<Tab[]>([]);
  let activeId = $state("");
  let ready = $state(false);
  let attentionByLeaf = $state<Record<string, AttentionState>>({});
  // leaf key → backend pane id (for cwd queries on save) and restored cwd.
  let paneIdByLeaf: Record<string, PaneId> = {};
  let cwdByLeaf = $state<Record<string, string | null>>({});
  let saveScrollbackEnabled = $state(false);
  let keymap = $state<Keymap | null>(null);
  let menuOpen = $state(false);
  // Initialised to the config default so the menu never shows an empty leader
  // if it is somehow opened before restore() resolves (R6).
  let leaderKey = $state("ctrl+a");
  let layoutEl: HTMLDivElement;
  let layoutW = $state(1000);
  let layoutH = $state(600);

  const activeTab = $derived(tabs.find((t) => t.id === activeId));
  const rects = $derived(
    activeTab
      ? computeRects(activeTab.tree, { x: 0, y: 0, w: layoutW, h: layoutH })
      : new Map(),
  );
  const dividerList = $derived(
    activeTab ? dividers(activeTab.tree, { x: 0, y: 0, w: layoutW, h: layoutH }) : [],
  );
  const allPanes = $derived(
    tabs.flatMap((t) => leaves(t.tree).map((l) => ({ tabId: t.id, key: l.key }))),
  );
  const tabViews = $derived(
    tabs.map((t, i) => ({
      id: t.id,
      title: `agent ${i + 1}`,
      attention: leaves(t.tree).some((l) => attentionByLeaf[l.key] === "raised"),
    })),
  );

  function setActiveTree(tree: Node, focus?: string) {
    tabs = tabs.map((t) =>
      t.id === activeId
        ? { ...t, tree, focusedLeafKey: focus ?? t.focusedLeafKey }
        : t,
    );
  }
  function setActiveFocus(key: string) {
    tabs = tabs.map((t) => (t.id === activeId ? { ...t, focusedLeafKey: key } : t));
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
      closeTab(activeId);
      return;
    }
    setActiveTree(tree, leaves(tree)[0]?.key);
  }
  function focusDir(dir: "left" | "right" | "up" | "down") {
    if (!activeTab) return;
    const n = neighbor(rects, activeTab.focusedLeafKey, dir);
    if (n) setActiveFocus(n);
  }
  function focusPane(tabId: string, key: string) {
    activeId = tabId;
    tabs = tabs.map((t) => (t.id === tabId ? { ...t, focusedLeafKey: key } : t));
  }
  function cycleAttention() {
    const raised: { tabId: string; key: string }[] = [];
    for (const t of tabs)
      for (const l of leaves(t.tree))
        if (attentionByLeaf[l.key] === "raised") raised.push({ tabId: t.id, key: l.key });
    if (raised.length === 0) return;
    const cur = raised.findIndex(
      (r) => r.tabId === activeId && r.key === activeTab?.focusedLeafKey,
    );
    const next = raised[(cur + 1) % raised.length];
    focusPane(next.tabId, next.key);
  }
  function newTab() {
    const t = makeTab();
    tabs = [...tabs, t];
    activeId = t.id;
  }
  function closeTab(id: string) {
    const idx = tabs.findIndex((t) => t.id === id);
    const next = tabs.filter((t) => t.id !== id);
    if (next.length === 0) {
      const t = makeTab();
      tabs = [t];
      activeId = t.id;
      return;
    }
    tabs = next;
    if (activeId === id) activeId = next[Math.max(0, idx - 1)].id;
  }

  // leader X closes the whole active tab. Closing a single-pane tab is no more
  // destructive than closing its last pane, so it proceeds immediately; a tab
  // with multiple live agent panes asks first, so a sticky-Shift mis-fire of
  // leader x → X cannot silently destroy several agents' work (KTD5/R4).
  let pendingCloseTab = $state<string | null>(null);
  // Null-safe: pendingCloseTab can briefly outlive its tab (e.g. closing the
  // tab's last pane by keyboard while the confirm is up), so never assert.
  const pendingCloseCount = $derived.by(() => {
    const t = tabs.find((tab) => tab.id === pendingCloseTab);
    return t ? leaves(t.tree).length : 0;
  });
  // Confirm before closing a tab with more than one pane, whichever entry
  // point asks (leader X or the tab-bar ×) — a multi-pane tab holds several
  // live agents and the close is irreversible (KTD5/R4). Single-pane tabs
  // close immediately. The two overlays are kept mutually exclusive (openMenu).
  function requestCloseTab(id: string = activeId) {
    const tab = tabs.find((t) => t.id === id);
    if (!tab) return;
    if (leaves(tab.tree).length > 1) {
      menuOpen = false;
      pendingCloseTab = id;
    } else {
      closeTab(id);
    }
  }
  function confirmCloseTab() {
    if (pendingCloseTab) closeTab(pendingCloseTab);
    pendingCloseTab = null;
  }
  function cancelCloseTab() {
    pendingCloseTab = null;
  }
  // Opening the menu dismisses any pending close-tab confirm, so at most one
  // overlay is ever up — otherwise both Escape capture listeners would fire.
  function openMenu() {
    pendingCloseTab = null;
    menuOpen = true;
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
    if (pendingCloseTab === null) return; // close-tab confirm: Enter / Escape
    return captureKeys((e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        cancelCloseTab();
      } else if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        confirmCloseTab();
      }
    });
  });

  function onAttention(key: string, state: AttentionState, _reason: AttentionReason | null) {
    attentionByLeaf = { ...attentionByLeaf, [key]: state };
  }
  function onSpawned(key: string, paneId: PaneId) {
    paneIdByLeaf[key] = paneId;
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

  // ---- persistence (U12) ---------------------------------------------------
  async function persist() {
    const savedTabs: SavedTab[] = [];
    for (const t of tabs) {
      const panes: Record<string, SavedPane> = {};
      for (const l of leaves(t.tree)) {
        const pid = paneIdByLeaf[l.key];
        const cwd = pid != null ? await paneCwd(pid) : (cwdByLeaf[l.key] ?? null);
        panes[l.key] = { cwd, title: null };
      }
      savedTabs.push({ tree: t.tree, panes });
    }
    await saveSession({
      tabs: savedTabs,
      activeIndex: Math.max(0, tabs.findIndex((t) => t.id === activeId)),
    });
  }

  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    // Re-run on any layout change; debounce the write (R13).
    void tabs;
    void activeId;
    if (!ready) return;
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => void persist(), 800);
  });

  async function restore() {
    const cfg = await getConfig();
    saveScrollbackEnabled = cfg.saveScrollback;
    leaderKey = cfg.leaderKey;
    keymap = new Keymap(cfg.leaderKey, {
      newTab,
      splitHorizontal: () => split("horizontal"),
      splitVertical: () => split("vertical"),
      closePane,
      closeTab: requestCloseTab,
      focusLeft: () => focusDir("left"),
      focusRight: () => focusDir("right"),
      focusUp: () => focusDir("up"),
      focusDown: () => focusDir("down"),
      cycleAttention,
      openMenu,
    });

    const saved = await loadSession();
    if (saved && saved.tabs.length) {
      // Keep saved keys (so scrollback files match) and bump the counter past
      // them so new nodes never collide.
      ensureKeyCounterAbove(saved.tabs.flatMap((t) => collectKeys(t.tree)));
      const cwds: Record<string, string | null> = {};
      for (const st of saved.tabs)
        for (const [k, p] of Object.entries(st.panes)) cwds[k] = p.cwd;
      cwdByLeaf = cwds;
      tabs = saved.tabs.map((st) => ({
        id: `tab-${nextTabId++}`,
        tree: st.tree,
        focusedLeafKey: leaves(st.tree)[0]?.key ?? "",
      }));
      activeId =
        tabs[Math.min(Math.max(0, saved.activeIndex), tabs.length - 1)]?.id ??
        tabs[0].id;
    } else {
      const t = makeTab();
      tabs = [t];
      activeId = t.id;
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
    reportForeground();
    void restore();
    return () => {
      window.removeEventListener("focus", reportForeground);
      window.removeEventListener("blur", reportForeground);
    };
  });
</script>

<div class="app">
  <TabBar
    tabs={tabViews}
    {activeId}
    onSelect={(id) => (activeId = id)}
    onClose={requestCloseTab}
    onNew={newTab}
    onSplitH={() => split("horizontal")}
    onSplitV={() => split("vertical")}
    onClosePane={closePane}
    onMenu={openMenu}
  />
  <div class="layout" bind:this={layoutEl} bind:clientWidth={layoutW} bind:clientHeight={layoutH}>
    {#each allPanes as p (p.key)}
      {@const r = p.tabId === activeId ? rects.get(p.key) : undefined}
      <div
        class="slot"
        class:hidden={!r}
        style={r ? `left:${r.x}px;top:${r.y}px;width:${r.w}px;height:${r.h}px` : ""}
      >
        <Terminal
          leafKey={p.key}
          focused={p.tabId === activeId && activeTab?.focusedLeafKey === p.key}
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

  {#if pendingCloseTab !== null}
    <div class="backdrop" role="presentation" onpointerdown={cancelCloseTab}>
      <div
        class="confirm"
        role="alertdialog"
        aria-label="Close tab confirmation"
        tabindex="-1"
        onpointerdown={(e) => e.stopPropagation()}
      >
        <p class="confirm-msg">
          Close this tab and its {pendingCloseCount} panes?
        </p>
        <div class="confirm-actions">
          <button class="btn danger" onclick={confirmCloseTab}>Close tab</button>
          <button class="btn" onclick={cancelCloseTab}>Cancel</button>
        </div>
        <p class="confirm-hint">Enter to close · Esc to cancel</p>
      </div>
    </div>
  {/if}

  <HotkeyMenu
    open={menuOpen}
    leader={leaderKey}
    onClose={() => (menuOpen = false)}
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
  .layout {
    position: relative;
    flex: 1;
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
