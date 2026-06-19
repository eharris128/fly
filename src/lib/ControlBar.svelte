<script lang="ts">
  // The top control bar (U16). Tabs moved into the workspace sidebar, so this is
  // now a slim strip: a sidebar toggle + a `workspace / tab` breadcrumb on the
  // left, and the pane controls (split / close-pane / hotkeys) on the right.
  interface Props {
    workspaceName: string;
    tabName: string;
    sidebarCollapsed: boolean;
    onToggleSidebar: () => void;
    onSplitH: () => void;
    onSplitV: () => void;
    onClosePane: () => void;
    onMenu: () => void;
  }
  let {
    workspaceName,
    tabName,
    sidebarCollapsed,
    onToggleSidebar,
    onSplitH,
    onSplitV,
    onClosePane,
    onMenu,
  }: Props = $props();
</script>

<div class="controlbar">
  <div class="left">
    <button
      class="iconbtn"
      title={sidebarCollapsed ? "show sidebar" : "hide sidebar"}
      onclick={onToggleSidebar}>{sidebarCollapsed ? "›" : "‹"}</button
    >
    <span class="crumb">
      <span class="ws">{workspaceName}</span>
      <span class="sep">/</span>
      <span class="tab">{tabName}</span>
    </span>
  </div>
  <div class="controls">
    <button class="iconbtn" title="split right" onclick={onSplitH}>▥</button>
    <button class="iconbtn" title="split down" onclick={onSplitV}>▤</button>
    <button class="iconbtn" title="close pane" onclick={onClosePane}>✕</button>
    <button class="iconbtn" title="hotkeys" onclick={onMenu}>?</button>
  </div>
</div>

<style>
  .controlbar {
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
  .left,
  .controls {
    display: flex;
    align-items: center;
  }
  .crumb {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0 6px;
    overflow: hidden;
    white-space: nowrap;
  }
  .crumb .ws {
    opacity: 0.6;
  }
  .crumb .sep {
    opacity: 0.35;
  }
  .crumb .tab {
    color: #4da3ff;
    overflow: hidden;
    text-overflow: ellipsis;
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
