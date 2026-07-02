<script lang="ts">
  // The notification panel (cmux ⌘⇧I → fly's `leader n`): a focus-taking overlay
  // listing recent notifications newest-first. Modeled on CommandPalette (it
  // takes DOM focus, so App restores terminal focus on close via
  // focusActivePane). A dumb view — App owns the history and passes the resolved
  // entries plus the action callbacks (KTD16).
  import { tick } from "svelte";
  import { relativeTime, type NotificationView } from "./notifications";
  import type { AttentionReason } from "../ipc";

  interface Props {
    open: boolean;
    entries: NotificationView[];
    onJump: (id: number) => void;
    onClear: (id: number) => void;
    onClearAll: () => void;
    onMarkAllRead: () => void;
    onClose: () => void;
  }
  let { open, entries, onJump, onClear, onClearAll, onMarkAllRead, onClose }: Props =
    $props();

  let selected = $state(0);
  let panelEl = $state<HTMLDivElement>();
  let listEl = $state<HTMLUListElement>();

  const REASON_LABEL: Record<AttentionReason, string> = {
    question: "waiting",
    permission: "permission",
    alert: "alert",
    finished: "finished",
    error: "error",
  };

  // Reset selection + take focus each time the panel opens. Reads only `open`.
  $effect(() => {
    if (open) {
      selected = 0;
      void tick().then(() => panelEl?.focus());
    }
  });

  // Keep the highlight in range as entries change (clearing shrinks the list).
  $effect(() => {
    if (selected >= entries.length) selected = Math.max(0, entries.length - 1);
  });

  function move(delta: number) {
    if (entries.length === 0) return;
    selected = (selected + delta + entries.length) % entries.length;
    void scrollSelectedIntoView();
  }
  async function scrollSelectedIntoView() {
    await tick();
    listEl
      ?.querySelector<HTMLElement>(`[data-i="${selected}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }

  // The panel holds DOM focus, so it handles its own keys; App's window-level
  // leader listener bails while it is open, so nothing double-fires.
  function onKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const entry = entries[selected];
      if (entry) onJump(entry.id);
    } else if (e.key === "Delete" || e.key === "Backspace") {
      e.preventDefault();
      const entry = entries[selected];
      if (entry) onClear(entry.id);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    }
  }
</script>

{#if open}
  <div class="backdrop" role="presentation" onpointerdown={onClose}>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      class="panel"
      bind:this={panelEl}
      role="dialog"
      aria-label="Notifications"
      tabindex="-1"
      onpointerdown={(e) => e.stopPropagation()}
      onkeydown={onKeydown}
    >
      <div class="head">
        <span class="ttl">Notifications</span>
        <div class="acts">
          <button class="link" onclick={onMarkAllRead} disabled={entries.length === 0}>
            Mark all read
          </button>
          <button class="link" onclick={onClearAll} disabled={entries.length === 0}>
            Clear all
          </button>
        </div>
      </div>
      <ul class="list" bind:this={listEl} role="listbox" tabindex="-1">
        {#each entries as n, i (n.id)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
          <li
            class="row reason-{n.reason}"
            class:sel={i === selected}
            class:unread={n.state === "unread"}
            data-i={i}
            role="option"
            aria-selected={i === selected}
            onpointermove={() => (selected = i)}
            onclick={() => onJump(n.id)}
          >
            <span class="tag">{REASON_LABEL[n.reason]}</span>
            <div class="mid">
              <div class="title">{n.title ?? REASON_LABEL[n.reason]}</div>
              {#if n.body}<div class="body">{n.body}</div>{/if}
              <div class="meta">
                <span class="src" class:closed={!n.jumpable}>{n.source}</span>
                <span class="dot">·</span>
                <span class="time">{relativeTime(n.ts, Date.now())}</span>
              </div>
            </div>
            <button
              class="x"
              aria-label="Clear notification"
              title="Clear"
              onclick={(e) => {
                e.stopPropagation();
                onClear(n.id);
              }}>×</button
            >
          </li>
        {/each}
        {#if entries.length === 0}
          <li class="empty">No pending notifications — opening a tab clears them</li>
        {/if}
      </ul>
      <div class="hint">↑↓ navigate · Enter jump · Del clear · Esc close</div>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: flex;
    align-items: flex-start;
    justify-content: flex-end;
    padding: 8px;
    background: rgba(5, 8, 16, 0.5);
  }
  .panel {
    display: flex;
    flex-direction: column;
    background: #0b1020;
    border: 1px solid #2b3a55;
    border-radius: 6px;
    width: 380px;
    max-width: 92vw;
    max-height: 80vh;
    color: #c9d1d9;
    font:
      13px/1.4 ui-monospace,
      monospace;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.55);
    overflow: hidden;
    outline: none;
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 9px 12px;
    border-bottom: 1px solid #161b2c;
    background: #0a0e1a;
  }
  .ttl {
    font-weight: 600;
  }
  .acts {
    display: flex;
    gap: 10px;
  }
  .link {
    background: none;
    border: none;
    color: #4da3ff;
    font: inherit;
    cursor: pointer;
    padding: 0;
    opacity: 0.85;
  }
  .link:hover {
    opacity: 1;
  }
  .link:disabled {
    color: #c9d1d9;
    opacity: 0.3;
    cursor: default;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 4px 0;
    overflow-y: auto;
  }
  .row {
    display: flex;
    align-items: flex-start;
    gap: 9px;
    padding: 8px 12px;
    cursor: pointer;
    border-left: 2px solid transparent;
  }
  .row.sel {
    background: #11182b;
  }
  .row.unread {
    border-left-color: #f5a623;
  }
  .tag {
    flex: none;
    margin-top: 1px;
    padding: 1px 7px;
    border-radius: 9px;
    font-size: 10px;
    font-weight: 600;
    color: #1a1205;
    background: #f5a623;
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .row.reason-error .tag {
    background: #f56262;
  }
  .row.reason-finished .tag {
    background: #4da3ff;
  }
  .row.reason-question .tag {
    background: #8ad17a;
  }
  .mid {
    flex: 1;
    min-width: 0;
  }
  .title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row.unread .title {
    font-weight: 600;
  }
  .body {
    opacity: 0.65;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    margin-top: 1px;
  }
  .meta {
    display: flex;
    gap: 5px;
    align-items: center;
    margin-top: 3px;
    font-size: 11px;
    opacity: 0.55;
  }
  .src {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 200px;
  }
  .src.closed {
    font-style: italic;
  }
  .x {
    flex: none;
    background: none;
    border: none;
    color: #c9d1d9;
    opacity: 0.35;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    padding: 0 2px;
  }
  .x:hover {
    opacity: 0.9;
  }
  .empty {
    padding: 16px 12px;
    text-align: center;
    opacity: 0.5;
  }
  .hint {
    padding: 6px 12px;
    border-top: 1px solid #161b2c;
    font-size: 11px;
    opacity: 0.45;
  }
</style>
