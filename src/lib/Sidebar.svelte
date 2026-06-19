<script lang="ts">
  // The workspace sidebar (U16): a cmux-style collapsible tree. Top-level rows
  // are workspaces; each expands to its named tabs. Purely presentational — the
  // parent (App.svelte) owns every piece of state, including the inline-rename
  // `editing` target, so the leader `r` chord can start a rename the same way a
  // double-click does. Per-workspace expand/collapse is the one bit of local UI
  // state; it isn't worth persisting, so it lives here.

  interface SidebarTab {
    id: string;
    title: string;
    attention: boolean;
  }
  interface SidebarWorkspace {
    id: string;
    name: string;
    attention: boolean;
    tabs: SidebarTab[];
  }
  type Editing = { kind: "tab" | "ws"; id: string } | null;

  interface Props {
    workspaces: SidebarWorkspace[];
    activeWorkspaceId: string;
    activeTabId: string;
    editing: Editing;
    onSelectTab: (wsId: string, tabId: string) => void;
    onCloseTab: (tabId: string) => void;
    onNewTab: (wsId: string) => void;
    onSelectWorkspace: (wsId: string) => void;
    onNewWorkspace: () => void;
    onDeleteWorkspace: (wsId: string) => void;
    onStartEdit: (kind: "tab" | "ws", id: string) => void;
    onCommitEdit: (name: string) => void;
    onCancelEdit: () => void;
    onToggleCollapsed: () => void;
  }
  let {
    workspaces,
    activeWorkspaceId,
    activeTabId,
    editing,
    onSelectTab,
    onCloseTab,
    onNewTab,
    onSelectWorkspace,
    onNewWorkspace,
    onDeleteWorkspace,
    onStartEdit,
    onCommitEdit,
    onCancelEdit,
    onToggleCollapsed,
  }: Props = $props();

  // Collapsed workspace ids (expand/collapse of the tab list). Default expanded.
  let folded = $state<Set<string>>(new Set());
  function toggleFold(id: string) {
    const next = new Set(folded);
    next.has(id) ? next.delete(id) : next.add(id);
    folded = next;
  }

  // Enter/Escape both unmount the input, which fires a trailing blur; suppress
  // that one blur so it can't override an explicit commit/cancel.
  let suppressBlur = false;
  function focusSelect(node: HTMLInputElement) {
    node.focus();
    node.select();
  }
  function onRenameKey(e: KeyboardEvent, value: string) {
    e.stopPropagation();
    if (e.key === "Enter") {
      e.preventDefault();
      suppressBlur = true;
      onCommitEdit(value);
    } else if (e.key === "Escape") {
      e.preventDefault();
      suppressBlur = true;
      onCancelEdit();
    }
  }
  function onRenameBlur(value: string) {
    if (suppressBlur) {
      suppressBlur = false;
      return;
    }
    onCommitEdit(value);
  }

  const isEditingTab = (id: string) =>
    editing?.kind === "tab" && editing.id === id;
  const isEditingWs = (id: string) =>
    editing?.kind === "ws" && editing.id === id;
</script>

<div class="sidebar">
  <div class="tree">
    {#each workspaces as ws (ws.id)}
      {@const open = !folded.has(ws.id)}
      <div class="ws" class:active={ws.id === activeWorkspaceId}>
        <div
          class="ws-head"
          role="button"
          tabindex="0"
          onclick={() => onSelectWorkspace(ws.id)}
          onkeydown={(e) =>
            (e.key === "Enter" || e.key === " ") && onSelectWorkspace(ws.id)}
        >
          <button
            class="twisty"
            title={open ? "collapse" : "expand"}
            onclick={(e) => {
              e.stopPropagation();
              toggleFold(ws.id);
            }}>{open ? "▾" : "▸"}</button
          >
          {#if isEditingWs(ws.id)}
            <input
              class="rename"
              value={ws.name}
              use:focusSelect
              onclick={(e) => e.stopPropagation()}
              onkeydown={(e) => onRenameKey(e, e.currentTarget.value)}
              onblur={(e) => onRenameBlur(e.currentTarget.value)}
            />
          {:else}
            <button
              type="button"
              class="ws-name"
              title="double-click to rename"
              ondblclick={(e) => {
                e.stopPropagation();
                onStartEdit("ws", ws.id);
              }}>{ws.name}</button
            >
          {/if}
          {#if ws.attention}
            <span class="dot" title="an agent needs attention"></span>
          {/if}
          <span class="ws-actions">
            <button
              class="icon"
              title="new tab"
              onclick={(e) => {
                e.stopPropagation();
                onNewTab(ws.id);
              }}>+</button
            >
            <button
              class="icon"
              title="delete workspace"
              onclick={(e) => {
                e.stopPropagation();
                onDeleteWorkspace(ws.id);
              }}>×</button
            >
          </span>
        </div>

        {#if open}
          <div class="tabs">
            {#each ws.tabs as tab (tab.id)}
              <div
                class="tab"
                class:active={tab.id === activeTabId &&
                  ws.id === activeWorkspaceId}
                role="button"
                tabindex="0"
                onclick={() => onSelectTab(ws.id, tab.id)}
                onkeydown={(e) =>
                  (e.key === "Enter" || e.key === " ") &&
                  onSelectTab(ws.id, tab.id)}
              >
                {#if tab.attention}
                  <span class="dot" title="an agent needs attention"></span>
                {:else}
                  <span class="dot-spacer"></span>
                {/if}
                {#if isEditingTab(tab.id)}
                  <input
                    class="rename"
                    value={tab.title}
                    use:focusSelect
                    onclick={(e) => e.stopPropagation()}
                    onkeydown={(e) => onRenameKey(e, e.currentTarget.value)}
                    onblur={(e) => onRenameBlur(e.currentTarget.value)}
                  />
                {:else}
                  <button
                    type="button"
                    class="tab-name"
                    title="double-click to rename"
                    ondblclick={(e) => {
                      e.stopPropagation();
                      onStartEdit("tab", tab.id);
                    }}>{tab.title}</button
                  >
                {/if}
                <button
                  class="icon close"
                  title="close tab"
                  onclick={(e) => {
                    e.stopPropagation();
                    onCloseTab(tab.id);
                  }}>×</button
                >
              </div>
            {/each}
          </div>
        {/if}
      </div>
    {/each}
  </div>

  <div class="footer">
    <button class="foot-btn" title="new workspace" onclick={onNewWorkspace}
      >+ workspace</button
    >
    <button
      class="foot-btn collapse"
      title="collapse sidebar"
      onclick={onToggleCollapsed}>‹</button
    >
  </div>
</div>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    width: 200px;
    flex: none;
    height: 100%;
    background: #0a0e1a;
    border-right: 1px solid #161b2c;
    user-select: none;
    font:
      12px/1.3 ui-monospace,
      monospace;
    color: #c9d1d9;
    overflow: hidden;
  }
  .tree {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 4px 0;
  }
  .ws-head {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 6px 4px 4px;
    cursor: pointer;
    opacity: 0.85;
  }
  .ws-head:hover {
    background: #11182b;
  }
  .ws.active > .ws-head {
    opacity: 1;
  }
  .ws.active > .ws-head .ws-name {
    color: #4da3ff;
  }
  .twisty {
    background: none;
    border: none;
    color: inherit;
    opacity: 0.7;
    cursor: pointer;
    font-size: 10px;
    width: 14px;
    padding: 0;
    line-height: 1;
  }
  .ws-name {
    flex: 1;
    min-width: 0;
    text-align: left;
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font: inherit;
    font-weight: bold;
    padding: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ws-actions {
    display: none;
    gap: 2px;
  }
  .ws-head:hover .ws-actions {
    display: flex;
  }
  .tabs {
    display: flex;
    flex-direction: column;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 5px;
    padding: 3px 6px 3px 18px;
    cursor: pointer;
    opacity: 0.7;
  }
  .tab:hover {
    background: #11182b;
  }
  .tab.active {
    opacity: 1;
    background: #0b1020;
    box-shadow: inset 2px 0 0 #4da3ff;
  }
  .tab-name {
    flex: 1;
    min-width: 0;
    text-align: left;
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font: inherit;
    padding: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .dot {
    width: 7px;
    height: 7px;
    flex: none;
    border-radius: 50%;
    background: #f5a623;
    box-shadow: 0 0 6px #f5a623;
  }
  .dot-spacer {
    width: 7px;
    flex: none;
  }
  .icon {
    background: none;
    border: none;
    color: inherit;
    opacity: 0.5;
    cursor: pointer;
    font-size: 13px;
    line-height: 1;
    padding: 0 3px;
  }
  .icon:hover {
    opacity: 1;
  }
  .close {
    display: none;
  }
  .tab:hover .close,
  .tab.active .close {
    display: inline;
  }
  .rename {
    flex: 1;
    min-width: 0;
    background: #0b1020;
    border: 1px solid #4da3ff;
    border-radius: 3px;
    color: #c9d1d9;
    font: inherit;
    padding: 1px 4px;
  }
  .rename:focus {
    outline: none;
  }
  .footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    border-top: 1px solid #161b2c;
    padding: 4px 6px;
  }
  .foot-btn {
    background: none;
    border: none;
    color: #c9d1d9;
    opacity: 0.6;
    cursor: pointer;
    font: inherit;
    padding: 4px 6px;
    border-radius: 4px;
  }
  .foot-btn:hover {
    opacity: 1;
    background: #11182b;
  }
  .collapse {
    font-size: 14px;
    padding: 2px 8px;
  }
</style>
