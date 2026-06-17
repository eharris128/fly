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
    type Node,
    type Orientation,
    type DividerRect,
  } from "./lib/layout";
  import {
    setWindowForeground,
    type AttentionState,
    type AttentionReason,
  } from "./ipc";
  import { Keymap } from "./lib/keymap";
  import { getConfig } from "./lib/config";

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

  const firstTab = makeTab();
  let tabs = $state<Tab[]>([firstTab]);
  let activeId = $state(firstTab.id);
  let attentionByLeaf = $state<Record<string, AttentionState>>({});
  let layoutEl: HTMLDivElement;
  let layoutW = $state(1000);
  let layoutH = $state(600);

  const activeTab = $derived(tabs.find((t) => t.id === activeId) ?? tabs[0]);
  const rects = $derived(
    computeRects(activeTab.tree, { x: 0, y: 0, w: layoutW, h: layoutH }),
  );
  const dividerList = $derived(
    dividers(activeTab.tree, { x: 0, y: 0, w: layoutW, h: layoutH }),
  );
  // All leaves across all tabs; keyed so panes survive splits and tab switches.
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
    const rect = rects.get(activeTab.focusedLeafKey);
    if (rect && !canSplit(rect, orientation)) return; // min-size clamp (R7)
    const res = splitLeaf(activeTab.tree, activeTab.focusedLeafKey, orientation);
    if (res) setActiveTree(res.tree, res.added.key);
  }
  function closePane() {
    const tree = removeLeaf(activeTab.tree, activeTab.focusedLeafKey);
    if (tree === null) {
      closeTab(activeId); // closing the last pane closes the tab
      return;
    }
    setActiveTree(tree, leaves(tree)[0]?.key);
  }
  function focusDir(dir: "left" | "right" | "up" | "down") {
    const n = neighbor(rects, activeTab.focusedLeafKey, dir);
    if (n) setActiveFocus(n);
  }
  function focusPane(tabId: string, key: string) {
    activeId = tabId;
    tabs = tabs.map((t) => (t.id === tabId ? { ...t, focusedLeafKey: key } : t));
  }
  /** Leader+u: jump to the next pane that needs attention (across tabs). */
  function cycleAttention() {
    const raised: { tabId: string; key: string }[] = [];
    for (const t of tabs)
      for (const l of leaves(t.tree))
        if (attentionByLeaf[l.key] === "raised") raised.push({ tabId: t.id, key: l.key });
    if (raised.length === 0) return;
    const cur = raised.findIndex(
      (r) => r.tabId === activeId && r.key === activeTab.focusedLeafKey,
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

  function onAttention(
    key: string,
    state: AttentionState,
    _reason: AttentionReason | null,
  ) {
    attentionByLeaf = { ...attentionByLeaf, [key]: state };
  }

  function startDrag(d: DividerRect, ev: PointerEvent) {
    ev.preventDefault();
    const horizontal = d.orientation === "horizontal";
    const move = (e: PointerEvent) => {
      const base = layoutEl.getBoundingClientRect();
      const ratio = horizontal
        ? (e.clientX - base.left - d.parent.x) / d.parent.w
        : (e.clientY - base.top - d.parent.y) / d.parent.h;
      setActiveTree(
        setRatio(activeTab.tree, d.splitKey, Math.min(0.9, Math.max(0.1, ratio))),
      );
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }

  let keymap = $state<Keymap | null>(null);

  // Window foreground feeds the attention suppression matrix (KTD8).
  function reportForeground() {
    void setWindowForeground(document.hasFocus());
  }
  onMount(() => {
    window.addEventListener("focus", reportForeground);
    window.addEventListener("blur", reportForeground);
    reportForeground();
    // Build the keymap once the leader key is loaded from config (U13).
    void getConfig().then((cfg) => {
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
    });
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
          focused={p.tabId === activeId && activeTab.focusedLeafKey === p.key}
          {keymap}
          onFocusRequest={setActiveFocus}
          onExit={() => {}}
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
