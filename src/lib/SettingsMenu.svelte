<script lang="ts" module>
  /** One boolean setting rendered as a labelled toggle row. `key` is opaque to
   * this component — it round-trips back through `onToggle` so App owns the
   * mapping to config fields. Adding a setting is one more entry in App's list. */
  export interface ToggleSetting {
    key: string;
    label: string;
    description?: string;
    value: boolean;
  }
</script>

<script lang="ts">
  // The settings menu (`leader ,`, the ⚙ control-bar button, or the command
  // palette): a focus-taking modal listing toggle settings. Modeled on
  // NotificationPanel / CommandPalette — it takes DOM focus, so App restores
  // terminal focus on close via focusActivePane. A dumb view: App owns the
  // settings values (seeded from and persisted to config) and passes the rows
  // plus a single toggle callback.
  import { tick } from "svelte";

  interface Props {
    open: boolean;
    toggles: ToggleSetting[];
    onToggle: (key: string, value: boolean) => void;
    onClose: () => void;
  }
  let { open, toggles, onToggle, onClose }: Props = $props();

  let selected = $state(0);
  let panelEl = $state<HTMLDivElement>();
  let listEl = $state<HTMLUListElement>();

  // Reset selection + take focus each time the menu opens. Reads only `open`.
  $effect(() => {
    if (open) {
      selected = 0;
      void tick().then(() => panelEl?.focus());
    }
  });

  // Keep the highlight in range if the row set changes.
  $effect(() => {
    if (selected >= toggles.length) selected = Math.max(0, toggles.length - 1);
  });

  function move(delta: number) {
    if (toggles.length === 0) return;
    selected = (selected + delta + toggles.length) % toggles.length;
    void scrollSelectedIntoView();
  }
  async function scrollSelectedIntoView() {
    await tick();
    listEl
      ?.querySelector<HTMLElement>(`[data-i="${selected}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }
  function toggle(row: ToggleSetting) {
    onToggle(row.key, !row.value);
  }

  // The menu holds DOM focus, so it handles its own keys; App's window-level
  // leader listener bails while it is open, so nothing double-fires.
  function onKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      const row = toggles[selected];
      if (row) toggle(row);
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
      aria-label="Settings"
      tabindex="-1"
      onpointerdown={(e) => e.stopPropagation()}
      onkeydown={onKeydown}
    >
      <div class="head">
        <span class="ttl">Settings</span>
      </div>
      <ul class="list" bind:this={listEl} role="listbox" tabindex="-1">
        {#each toggles as row, i (row.key)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
          <li
            class="row"
            class:sel={i === selected}
            data-i={i}
            role="option"
            aria-selected={i === selected}
            onpointermove={() => (selected = i)}
            onclick={() => toggle(row)}
          >
            <div class="mid">
              <div class="label">{row.label}</div>
              {#if row.description}<div class="desc">{row.description}</div>{/if}
            </div>
            <button
              class="switch"
              class:on={row.value}
              role="switch"
              aria-checked={row.value}
              aria-label={row.label}
              onclick={(e) => {
                e.stopPropagation();
                toggle(row);
              }}
            >
              <span class="knob"></span>
            </button>
          </li>
        {/each}
        {#if toggles.length === 0}
          <li class="empty">No settings yet</li>
        {/if}
      </ul>
      <div class="hint">↑↓ navigate · Space toggle · Esc close</div>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 8px;
    background: var(--color-overlay);
  }
  .panel {
    display: flex;
    flex-direction: column;
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    width: 420px;
    max-width: 92vw;
    max-height: 80vh;
    color: var(--color-text);
    font-family: var(--font-sans);
    font-size: var(--font-size-sm);
    line-height: 1.4;
    box-shadow: var(--shadow-lg);
    overflow: hidden;
    outline: none;
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 9px 12px;
    border-bottom: 1px solid var(--color-border);
    background: var(--color-bg-subtle);
  }
  .ttl {
    font-weight: 600;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 4px 0;
    overflow-y: auto;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 12px;
    cursor: pointer;
    border-left: 2px solid transparent;
  }
  .row.sel {
    background: var(--color-surface);
    border-left-color: var(--color-accent);
  }
  .mid {
    flex: 1;
    min-width: 0;
  }
  .label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .desc {
    opacity: 0.6;
    font-size: 15px;
    margin-top: 2px;
  }
  .switch {
    flex: none;
    position: relative;
    width: 34px;
    height: 18px;
    border-radius: 9px;
    border: 1px solid var(--color-border);
    background: var(--color-surface);
    cursor: pointer;
    padding: 0;
    transition:
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .switch.on {
    background: var(--color-accent);
    border-color: var(--color-accent);
  }
  .knob {
    position: absolute;
    top: 1px;
    left: 1px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--color-text);
    transition: transform 0.12s ease;
  }
  .switch.on .knob {
    transform: translateX(16px);
    background: var(--color-surface);
  }
  .empty {
    padding: 16px 12px;
    text-align: center;
    opacity: 0.5;
  }
  .hint {
    padding: 6px 12px;
    border-top: 1px solid var(--color-border);
    font-size: 14px;
    opacity: 0.45;
  }
</style>
