<script lang="ts">
  // The workspace sidebar (U16): a cmux-style collapsible tree. Top-level rows
  // are workspaces; each expands to its named tabs. Purely presentational — the
  // parent (App.svelte) owns every piece of state, including the inline-rename
  // `editing` target, so the leader `r` chord can start a rename the same way a
  // double-click does. Per-workspace expand/collapse is the one bit of local UI
  // state; it isn't worth persisting, so it lives here.
  //
  // Workspaces can be reordered by dragging (U3). The pure index math lives in
  // workspaces.ts (insertionIndex); this component wires it to pointer events,
  // mirroring App.svelte's split-divider `startDrag`. Order persistence is the
  // parent's job — the reorder fires `onReorderWorkspace(from, to)`.
  import GripVertical from "@lucide/svelte/icons/grip-vertical";
  import ChevronDown from "@lucide/svelte/icons/chevron-down";
  import ChevronRight from "@lucide/svelte/icons/chevron-right";
  import VolumeX from "@lucide/svelte/icons/volume-x";
  import Volume2 from "@lucide/svelte/icons/volume-2";
  import Plus from "@lucide/svelte/icons/plus";
  import X from "@lucide/svelte/icons/x";
  import { useIconDefaults } from "./icons";
  import { insertionIndex, type AttentionKind } from "./workspaces";

  useIconDefaults(14);

  interface SidebarTab {
    id: string;
    title: string;
    attention: AttentionKind | null;
    unread: number;
  }
  interface SidebarWorkspace {
    id: string;
    name: string;
    attention: AttentionKind | null;
    unread: number;
    muted: boolean;
    tabs: SidebarTab[];
  }

  // The dot's two-way cue (see workspaces.attentionKind): amber = an agent is
  // blocked on your input; blue = it finished and has a result waiting.
  const DOT_TITLE: Record<AttentionKind, string> = {
    input: "an agent needs your input",
    done: "an agent finished — result ready",
  };
  // `pane` is App's over-the-pane label editor — never rendered here, but the
  // shared `editing` slot can hold it, so the type must admit it.
  type Editing = { kind: "tab" | "ws" | "pane"; id: string } | null;

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
    onToggleWorkspaceMute: (wsId: string) => void;
    onReorderWorkspace: (fromIndex: number, toIndex: number) => void;
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
    onToggleWorkspaceMute,
    onReorderWorkspace,
  }: Props = $props();

  // Collapsed workspace ids (expand/collapse of the tab list). Default expanded.
  let folded = $state<Set<string>>(new Set());
  function toggleFold(id: string) {
    const next = new Set(folded);
    next.has(id) ? next.delete(id) : next.add(id);
    folded = next;
  }

  // ---- workspace drag-reorder (U3) -----------------------------------------
  // Transient, drag-only UI state — like `folded`, not worth persisting. The
  // dragged row is dimmed; a thin accent line marks the drop position.
  let treeEl: HTMLDivElement;
  let draggedIndex = $state<number | null>(null);
  let dropIndex = $state<number | null>(null);
  // Show the insertion line only when the drop would actually move the row.
  const showDrop = $derived(
    draggedIndex !== null && dropIndex !== null && dropIndex !== draggedIndex,
  );
  // The post-removal `dropIndex` maps to a gap among the current (still-rendered)
  // rows: landing below the dragged row sits one slot further down visually.
  const visualGap = $derived(
    dropIndex === null || draggedIndex === null
      ? -1
      : dropIndex < draggedIndex
        ? dropIndex
        : dropIndex + 1,
  );

  function wsMidpoints(): number[] {
    if (!treeEl) return [];
    const rows = treeEl.querySelectorAll<HTMLElement>(":scope > .ws");
    return Array.from(rows, (el) => {
      const r = el.getBoundingClientRect();
      return r.top + r.height / 2;
    });
  }
  function startWsDrag(index: number, ev: PointerEvent) {
    ev.preventDefault();
    ev.stopPropagation();
    draggedIndex = index;
    dropIndex = index;
    const move = (e: PointerEvent) => {
      if (draggedIndex === null) return;
      dropIndex = insertionIndex(e.clientY, wsMidpoints(), draggedIndex);
    };
    const finish = (commit: boolean) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      window.removeEventListener("keydown", onKey, true);
      if (commit && draggedIndex !== null && dropIndex !== null) {
        onReorderWorkspace(draggedIndex, dropIndex); // no-ops if unchanged
      }
      draggedIndex = null;
      dropIndex = null;
    };
    const up = () => finish(true);
    const cancel = () => finish(false);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        finish(false); // drop-cancel: leave order untouched, clear the indicator
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", cancel);
    window.addEventListener("keydown", onKey, true);
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
  <div class="tree" bind:this={treeEl}>
    {#each workspaces as ws, i (ws.id)}
      {@const open = !folded.has(ws.id)}
      {#if showDrop && visualGap === i}
        <div class="drop-line" aria-hidden="true"></div>
      {/if}
      <div
        class="ws"
        class:active={ws.id === activeWorkspaceId}
        class:muted={ws.muted}
        class:dragging={draggedIndex === i}
      >
        <div
          class="ws-head"
          role="button"
          tabindex="0"
          onclick={() => onSelectWorkspace(ws.id)}
          onkeydown={(e) =>
            (e.key === "Enter" || e.key === " ") && onSelectWorkspace(ws.id)}
        >
          <button
            type="button"
            class="grip"
            title="drag to reorder"
            aria-label="drag to reorder workspace"
            onpointerdown={(e) => startWsDrag(i, e)}
            onclick={(e) => e.stopPropagation()}><GripVertical /></button
          >
          <button
            class="twisty"
            title={open ? "collapse" : "expand"}
            onclick={(e) => {
              e.stopPropagation();
              toggleFold(ws.id);
            }}>{#if open}<ChevronDown />{:else}<ChevronRight />{/if}</button
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
            <span
              class="dot"
              class:done={ws.attention === "done"}
              title={DOT_TITLE[ws.attention]}
            ></span>
          {/if}
          {#if ws.unread > 0}
            <span class="count" title="{ws.unread} unread">{ws.unread}</span>
          {/if}
          {#if ws.muted}
            <span class="muted-ind" title="workspace muted"><VolumeX /></span>
          {/if}
          <span class="ws-actions">
            <button
              class="icon"
              title="new tab"
              onclick={(e) => {
                e.stopPropagation();
                onNewTab(ws.id);
              }}><Plus /></button
            >
            <button
              class="icon"
              title={ws.muted ? "unmute workspace" : "mute workspace"}
              onclick={(e) => {
                e.stopPropagation();
                onToggleWorkspaceMute(ws.id);
              }}>{#if ws.muted}<Volume2 />{:else}<VolumeX />{/if}</button
            >
            <button
              class="icon"
              title="delete workspace"
              onclick={(e) => {
                e.stopPropagation();
                onDeleteWorkspace(ws.id);
              }}><X /></button
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
                  <span
                    class="dot"
                    class:done={tab.attention === "done"}
                    title={DOT_TITLE[tab.attention]}
                  ></span>
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
                {#if tab.unread > 0}
                  <span class="count" title="{tab.unread} unread">{tab.unread}</span>
                {/if}
                <button
                  class="icon close"
                  title="close tab"
                  onclick={(e) => {
                    e.stopPropagation();
                    onCloseTab(tab.id);
                  }}><X /></button
                >
              </div>
            {/each}
          </div>
        {/if}
      </div>
    {/each}
    {#if showDrop && visualGap === workspaces.length}
      <div class="drop-line" aria-hidden="true"></div>
    {/if}
  </div>

  <div class="footer">
    <button class="foot-btn" title="new workspace" onclick={onNewWorkspace}
      ><Plus /> workspace</button
    >
  </div>
</div>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    width: var(--size-sidebar);
    flex: none;
    height: 100%;
    background: var(--color-bg-subtle);
    border-right: var(--border-width) solid var(--color-border);
    user-select: none;
    font-family: var(--font-sans);
    font-size: var(--font-size-sm);
    line-height: 1.3;
    color: var(--color-text);
    overflow: hidden;
  }
  .tree {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--space-2);
  }
  .ws-head {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: var(--space-1) var(--space-2) var(--space-1) var(--space-1);
    border-radius: var(--radius-md);
    cursor: pointer;
    color: var(--color-text-muted);
  }
  .ws-head:hover {
    background: var(--color-surface);
  }
  .ws.active > .ws-head,
  .ws.active > .ws-head .ws-name {
    color: var(--color-text);
  }
  .twisty {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    color: inherit;
    opacity: 0.7;
    cursor: pointer;
    width: var(--space-5);
    padding: 0;
    line-height: 1;
  }
  /* Drag handle: faint by default (discoverable), brighter on row hover. */
  .grip {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    color: inherit;
    opacity: 0.25;
    cursor: grab;
    line-height: 1;
    width: var(--size-icon);
    flex: none;
    padding: 0;
    touch-action: none;
  }
  .ws-head:hover .grip {
    opacity: 0.7;
  }
  .grip:active {
    cursor: grabbing;
  }
  .ws.dragging {
    opacity: 0.4;
  }
  /* Insertion indicator between rows while dragging. */
  .drop-line {
    height: var(--border-width-strong);
    margin: var(--space-0) var(--space-2);
    background: var(--color-accent);
    border-radius: var(--radius-sm);
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
    font-weight: var(--font-weight-semibold);
    padding: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ws-actions {
    display: none;
    gap: var(--space-1);
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
    gap: var(--space-1);
    margin-left: var(--space-4);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-md);
    cursor: pointer;
    color: var(--color-text-muted);
  }
  .tab:hover {
    background: var(--color-surface);
  }
  .tab.active {
    color: var(--color-text);
    background: var(--color-accent-subtle);
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
  /* Amber = an agent is blocked on your input (question/permission — and any
     non-finished raise, which must read urgent). Blue = finished, result ready
     — the calmer cue, matching the notification panel's "finished" tag. */
  .dot {
    width: var(--space-2);
    height: var(--space-2);
    flex: none;
    border-radius: 50%;
    background: var(--color-warning);
    box-shadow: 0 0 var(--space-2) var(--color-warning);
  }
  .dot.done {
    background: var(--color-accent);
    box-shadow: 0 0 var(--space-2) var(--color-accent);
  }
  .dot-spacer {
    width: var(--space-2);
    flex: none;
  }
  /* Unread-notification count — a distinct cue from the amber raised dot. */
  .count {
    flex: none;
    min-width: var(--space-5);
    height: var(--space-5);
    padding: var(--space-0) var(--space-1);
    border-radius: var(--radius-full);
    background: var(--color-bg);
    border: var(--border-width) solid var(--color-border);
    color: var(--color-text-muted);
    font-size: var(--font-size-xs);
    font-weight: var(--font-weight-semibold);
    line-height: var(--space-5);
    text-align: center;
  }
  .muted-ind {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex: none;
    line-height: 1;
    opacity: 0.85;
  }
  /* Muted precedence: a muted workspace reads as muted even with raised agents. */
  .ws.muted > .ws-head .ws-name {
    opacity: 0.5;
  }
  .icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    color: inherit;
    opacity: 0.5;
    cursor: pointer;
    line-height: 1;
    padding: var(--space-0) var(--space-1);
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
    background: var(--color-surface);
    border: var(--border-width) solid var(--color-accent);
    border-radius: var(--radius-sm);
    color: var(--color-text);
    font: inherit;
    padding: var(--space-1);
  }
  .rename:focus {
    outline: none;
  }
  .footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    border-top: var(--border-width) solid var(--color-border);
    padding: var(--space-2) var(--space-3);
  }
  .foot-btn {
    gap: var(--space-1);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: none;
    border: none;
    color: var(--color-text-muted);
    cursor: pointer;
    font: inherit;
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-md);
  }
  .foot-btn:hover {
    color: var(--color-text);
    background: var(--color-surface);
  }
</style>
