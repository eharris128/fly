<script lang="ts">
  import { onMount } from "svelte";
  import Terminal from "./lib/Terminal.svelte";
  import TabBar from "./lib/TabBar.svelte";
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
    keymap = new Keymap(cfg.leaderKey, {
      newTab,
      splitHorizontal: () => split("horizontal"),
      splitVertical: () => split("vertical"),
      closePane,
      focusLeft: () => focusDir("left"),
      focusRight: () => focusDir("right"),
      focusUp: () => focusDir("up"),
      focusDown: () => focusDir("down"),
      cycleAttention,
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
    onClose={closeTab}
    onNew={newTab}
    onSplitH={() => split("horizontal")}
    onSplitV={() => split("vertical")}
    onClosePane={closePane}
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
</style>
