<script lang="ts">
  interface TabView {
    id: string;
    title: string;
    attention: boolean;
  }
  interface Props {
    tabs: TabView[];
    activeId: string;
    onSelect: (id: string) => void;
    onClose: (id: string) => void;
    onNew: () => void;
    onSplitH: () => void;
    onSplitV: () => void;
    onClosePane: () => void;
  }
  let {
    tabs,
    activeId,
    onSelect,
    onClose,
    onNew,
    onSplitH,
    onSplitV,
    onClosePane,
  }: Props = $props();
</script>

<div class="tabbar">
  <div class="tabs">
    {#each tabs as tab (tab.id)}
      <div
        class="tab"
        class:active={tab.id === activeId}
        role="tab"
        tabindex="0"
        aria-selected={tab.id === activeId}
        onclick={() => onSelect(tab.id)}
        onkeydown={(e) => (e.key === "Enter" || e.key === " ") && onSelect(tab.id)}
      >
        {#if tab.attention}
          <span class="dot" title="an agent needs attention"></span>
        {/if}
        <span class="title">{tab.title}</span>
        <button
          class="close"
          title="close tab"
          onclick={(e) => {
            e.stopPropagation();
            onClose(tab.id);
          }}>×</button
        >
      </div>
    {/each}
    <button class="iconbtn" title="new tab" onclick={onNew}>+</button>
  </div>
  <div class="controls">
    <button class="iconbtn" title="split right" onclick={onSplitH}>▥</button>
    <button class="iconbtn" title="split down" onclick={onSplitV}>▤</button>
    <button class="iconbtn" title="close pane" onclick={onClosePane}>✕</button>
  </div>
</div>

<style>
  .tabbar {
    display: flex;
    align-items: stretch;
    justify-content: space-between;
    height: 34px;
    background: #0a0e1a;
    border-bottom: 1px solid #161b2c;
    user-select: none;
    font:
      12px/1 ui-monospace,
      monospace;
    color: #c9d1d9;
  }
  .tabs,
  .controls {
    display: flex;
    align-items: center;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0 10px;
    height: 100%;
    cursor: pointer;
    border-right: 1px solid #161b2c;
    opacity: 0.6;
  }
  .tab.active {
    opacity: 1;
    background: #0b1020;
    box-shadow: inset 0 -2px 0 #4da3ff;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #f5a623;
    box-shadow: 0 0 6px #f5a623;
  }
  .close {
    background: none;
    border: none;
    color: inherit;
    opacity: 0.5;
    cursor: pointer;
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
  }
  .close:hover {
    opacity: 1;
  }
  .iconbtn {
    background: none;
    border: none;
    color: #c9d1d9;
    opacity: 0.6;
    cursor: pointer;
    font-size: 14px;
    padding: 0 8px;
    height: 100%;
  }
  .iconbtn:hover {
    opacity: 1;
    background: #11182b;
  }
</style>
