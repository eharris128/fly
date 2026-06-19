<script lang="ts">
  // The command palette (a U4 follow-up to the passive cheat-sheet): a
  // type-to-filter, Enter-to-run overlay. Unlike HotkeyMenu it DOES take DOM
  // focus (a deliberate departure from KTD3 — it needs a search field), so the
  // parent restores terminal focus when it closes (see App.focusActivePane).
  // Commands come in via props and render keyed by id; this component owns only
  // the query, the selection, and keyboard navigation.
  import { tick } from "svelte";
  import { filterCommands, type PaletteCommand } from "./palette";

  interface Props {
    open: boolean;
    commands: PaletteCommand[];
    onRun: (cmd: PaletteCommand) => void;
    onClose: () => void;
  }
  let { open, commands, onRun, onClose }: Props = $props();

  let query = $state("");
  let selected = $state(0);
  let inputEl = $state<HTMLInputElement>();
  let listEl = $state<HTMLUListElement>();

  const results = $derived(filterCommands(commands, query));

  // Reset and focus the field each time the palette opens. Reads only `open`,
  // so writing query/selected here never re-triggers it.
  $effect(() => {
    if (open) {
      query = "";
      selected = 0;
      void tick().then(() => inputEl?.focus());
    }
  });

  // Snap the highlight back to the top result whenever the query changes (the
  // standard palette feel). Depends on `query` only — arrow-key moves that
  // change `selected` don't re-run it.
  $effect(() => {
    void query;
    selected = 0;
  });

  function move(delta: number) {
    if (results.length === 0) return;
    selected = (selected + delta + results.length) % results.length;
    void scrollSelectedIntoView();
  }

  async function scrollSelectedIntoView() {
    await tick();
    listEl
      ?.querySelector<HTMLElement>(`[data-i="${selected}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }

  function runSelected() {
    const cmd = results[selected];
    if (cmd) onRun(cmd);
  }

  // The palette input is focused, so it handles its own keys; App's window-level
  // leader listener bails while the palette is open, so nothing double-fires.
  function onKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      runSelected();
    } else if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    }
  }
</script>

{#if open}
  <div class="backdrop" role="presentation" onpointerdown={onClose}>
    <div
      class="palette"
      role="dialog"
      aria-label="Command palette"
      tabindex="-1"
      onpointerdown={(e) => e.stopPropagation()}
    >
      <input
        bind:this={inputEl}
        bind:value={query}
        class="query"
        type="text"
        placeholder="Run a command, or jump to a workspace / tab…"
        spellcheck="false"
        autocomplete="off"
        autocapitalize="off"
        onkeydown={onKeydown}
      />
      <ul class="results" bind:this={listEl} role="listbox" tabindex="-1">
        {#each results as cmd, i (cmd.id)}
          <!-- Rows are a pointer affordance; keyboard selection lives on the
               combobox input (arrows + Enter), so the row needs no key handler.
               Full AT support is deferred (KTD6). -->
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <li
            class="row"
            class:sel={i === selected}
            data-i={i}
            role="option"
            aria-selected={i === selected}
            onpointermove={() => (selected = i)}
            onclick={() => onRun(cmd)}
          >
            <span class="title">{cmd.title}</span>
            {#if cmd.hint}<span class="hint">{cmd.hint}</span>{/if}
          </li>
        {/each}
        {#if results.length === 0}
          <li class="empty">No matching commands</li>
        {/if}
      </ul>
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
    background: rgba(5, 8, 16, 0.6);
  }
  .palette {
    display: flex;
    flex-direction: column;
    background: #0b1020;
    border: 1px solid #2b3a55;
    border-radius: 6px;
    width: 540px;
    max-width: 90vw;
    max-height: 60vh;
    color: #c9d1d9;
    font:
      13px/1.4 ui-monospace,
      monospace;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.55);
    overflow: hidden;
  }
  .query {
    border: none;
    border-bottom: 1px solid #161b2c;
    background: #0a0e1a;
    color: #c9d1d9;
    font: inherit;
    padding: 11px 14px;
    outline: none;
  }
  .query::placeholder {
    color: #c9d1d9;
    opacity: 0.4;
  }
  .results {
    list-style: none;
    margin: 0;
    padding: 6px 0;
    overflow-y: auto;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    padding: 6px 14px;
    cursor: pointer;
  }
  .row.sel {
    background: #11182b;
  }
  .row.sel .title {
    color: #4da3ff;
  }
  .title {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hint {
    flex: none;
    opacity: 0.5;
    font-size: 12px;
  }
  .empty {
    padding: 10px 14px;
    opacity: 0.5;
  }
</style>
