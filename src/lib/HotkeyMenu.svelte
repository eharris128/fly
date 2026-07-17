<script lang="ts">
  // The hotkey menu (U4): a passive cheat-sheet overlay listing every leader
  // chord. It renders straight from BINDINGS (the same array dispatch() uses),
  // so it can never drift from the real bindings (R3/KTD1). The parent owns the
  // open state and dismissal; this component is purely presentational and takes
  // no DOM focus (KTD3) — Escape is handled by the parent's window listener.
  import { BINDINGS, DIGIT_CHORD, LEADER_KEY, formatLeader, type Binding } from "./keymap";

  interface Props {
    open: boolean;
    leader: string;
    onClose: () => void;
  }
  let { open, leader, onClose }: Props = $props();

  const formattedLeader = $derived(formatLeader(leader));

  // keys[0] is the canonical display key; aliases (keys[1..], e.g. \ for |)
  // are suppressed. Cased to match how the chord is typed (X vs x).
  function keyLabel(b: Binding): string {
    const k = b.keys[0];
    // The double-tap binding's key IS the leader (U10) — render the live
    // leader so a remap keeps the row truthful.
    if (k === LEADER_KEY) return formattedLeader;
    return b.upper ? k.toUpperCase() : k;
  }
</script>

{#if open}
  <div class="backdrop" role="presentation" onpointerdown={onClose}>
    <!-- aria-hidden: the chords are all operable without this reference
         surface, and KTD3 keeps focus in the terminal, so a focus-less dialog
         would mislead assistive tech. Full AT support is deferred (KTD6). -->
    <div
      class="menu"
      aria-hidden="true"
      onpointerdown={(e) => e.stopPropagation()}
    >
      <div class="head">
        <span class="title">Hotkeys</span>
        <span class="leader">Leader&nbsp;<kbd>{formattedLeader}</kbd></span>
        <button class="close" title="close" onclick={onClose}>×</button>
      </div>
      <ul class="rows">
        {#each BINDINGS as b (b.label)}
          <li class="row">
            <span class="keys">
              <kbd>{formattedLeader}</kbd>
              <kbd>{keyLabel(b)}</kbd>
            </span>
            <span class="label">{b.label}</span>
          </li>
        {/each}
        <!-- The digit tab-switch chord (U1) isn't a BINDINGS entry (its action
             is parameterized), so it's documented here from DIGIT_CHORD. -->
        <li class="row">
          <span class="keys">
            <kbd>{formattedLeader}</kbd>
            <kbd>{DIGIT_CHORD.key}</kbd>
          </span>
          <span class="label">{DIGIT_CHORD.label}</span>
        </li>
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
    align-items: center;
    justify-content: center;
    background: rgba(5, 8, 16, 0.6);
  }
  .menu {
    display: flex;
    flex-direction: column;
    background: #0b1020;
    border: 1px solid #2b3a55;
    border-radius: 6px;
    min-width: 320px;
    max-width: 90vw;
    max-height: 70vh;
    color: #c9d1d9;
    font:
      12px/1 ui-monospace,
      monospace;
    box-shadow: 0 8px 30px rgba(0, 0, 0, 0.5);
    overflow: hidden;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 14px;
    border-bottom: 1px solid #161b2c;
    background: #0a0e1a;
  }
  .title {
    font-weight: bold;
    color: #4da3ff;
  }
  .leader {
    flex: 1;
    opacity: 0.7;
  }
  .close {
    background: none;
    border: none;
    color: inherit;
    opacity: 0.5;
    cursor: pointer;
    font-size: 16px;
    line-height: 1;
    padding: 0 2px;
  }
  .close:hover {
    opacity: 1;
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 6px 0;
    overflow-y: auto;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 5px 14px;
  }
  .keys {
    display: flex;
    gap: 4px;
    min-width: 110px;
  }
  .label {
    opacity: 0.85;
  }
  kbd {
    display: inline-block;
    background: #11182b;
    border: 1px solid #2b3a55;
    border-radius: 3px;
    padding: 2px 6px;
    font: inherit;
    color: #c9d1d9;
  }
</style>
