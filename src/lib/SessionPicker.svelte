<script lang="ts">
  // The session pick-list (fix-session-pane-attribution U6; R6/R7/R8): a
  // focus-taking overlay listing the cwd's qualifying sessions when handoff
  // can't resolve the pane's own session with confidence. Modeled on
  // NotificationPanel (backdrop + focused dialog, ↑↓/Enter/Esc; App restores
  // terminal focus on close via focusActivePane). A dumb view — App owns the
  // candidates and the awaited promise; rows come from the pure view-model
  // (session-picker.ts).
  import { tick } from "svelte";
  import { clampIndex, type SessionPickerRow } from "./session-picker";

  interface Props {
    open: boolean;
    rows: SessionPickerRow[];
    /** Divergence / force-re-pick context line; null for a plain ambiguous
     *  launch (KTD2/KTD4). */
    subtitle: string | null;
    onPick: (index: number) => void;
    onClose: () => void;
  }
  let { open, rows, subtitle, onPick, onClose }: Props = $props();

  let selected = $state(0);
  let panelEl = $state<HTMLDivElement>();
  let listEl = $state<HTMLUListElement>();

  // Reset selection + take focus each time the picker opens. Reads only `open`.
  $effect(() => {
    if (open) {
      selected = 0;
      void tick().then(() => panelEl?.focus());
    }
  });

  // Keep the highlight in range if the row set changes while open.
  $effect(() => {
    selected = clampIndex(selected, rows.length);
  });

  function move(delta: number) {
    if (rows.length === 0) return;
    selected = (selected + delta + rows.length) % rows.length;
    void scrollSelectedIntoView();
  }
  async function scrollSelectedIntoView() {
    await tick();
    listEl
      ?.querySelector<HTMLElement>(`[data-i="${selected}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }

  // The picker holds DOM focus, so it handles its own keys; App's window-level
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
      if (rows.length > 0) onPick(selected);
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
      aria-label="Pick a session to hand off"
      tabindex="-1"
      onpointerdown={(e) => e.stopPropagation()}
      onkeydown={onKeydown}
    >
      <div class="head">
        <span class="ttl">Pick the session to hand off</span>
      </div>
      {#if subtitle}
        <div class="sub">{subtitle}</div>
      {/if}
      <ul class="list" bind:this={listEl} role="listbox" tabindex="-1">
        {#each rows as row, i (row.shortId + i)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
          <li
            class="row"
            class:sel={i === selected}
            data-i={i}
            role="option"
            aria-selected={i === selected}
            onpointermove={() => (selected = i)}
            onclick={() => onPick(i)}
          >
            <div class="top">
              <span class="id">{row.shortId}</span>
              <span class="time">{row.when}</span>
            </div>
            <div class="snippet">{row.snippet}</div>
          </li>
        {/each}
      </ul>
      <div class="hint">↑↓ navigate · Enter hand off · Esc cancel</div>
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
    justify-content: center;
    padding-top: 12vh;
    background: rgba(5, 8, 16, 0.5);
  }
  .panel {
    display: flex;
    flex-direction: column;
    background: #0b1020;
    border: 1px solid #2b3a55;
    border-radius: 6px;
    width: 460px;
    max-width: 92vw;
    max-height: 70vh;
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
    padding: 9px 12px;
    border-bottom: 1px solid #161b2c;
    background: #0a0e1a;
  }
  .ttl {
    font-weight: 600;
  }
  .sub {
    padding: 7px 12px;
    font-size: 12px;
    color: #f5a623;
    border-bottom: 1px solid #161b2c;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 4px 0;
    overflow-y: auto;
  }
  .row {
    padding: 8px 12px;
    cursor: pointer;
    border-left: 2px solid transparent;
  }
  .row.sel {
    background: #11182b;
    border-left-color: #4da3ff;
  }
  .top {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
  }
  .id {
    font-weight: 600;
    color: #4da3ff;
  }
  .time {
    font-size: 11px;
    opacity: 0.55;
  }
  .snippet {
    margin-top: 2px;
    font-size: 12px;
    opacity: 0.7;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hint {
    padding: 6px 12px;
    border-top: 1px solid #161b2c;
    font-size: 11px;
    opacity: 0.45;
  }
</style>
