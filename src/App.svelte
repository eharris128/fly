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

  // Window foreground feeds the attention suppression matrix (KTD8).
  function reportForeground() {
    void setWindowForeground(document.hasFocus());
  }
  onMount(() => {
    window.addEventListener("focus", reportForeground);
    window.addEventListener("blur", reportForeground);
    reportForeground();
    return () => {
      window.removeEventListener("focus", reportForeground);
      window.removeEventListener("blur", reportForeground);
    };
  });

  // Suppress "unused" until the keyboard layer (U6) wires these.
  void focusDir;
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
