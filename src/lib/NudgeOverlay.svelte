<script lang="ts">
  // The attention-triage "move along" nudge (U6). Presentational and takes NO DOM
  // focus (the HotkeyMenu archetype, KTD2) so the focused xterm keeps focus and
  // type-through works (R14); App owns the Tab/Esc/passthrough key handling via a
  // captureKeys effect. Confined to the focused pane's slot, pinned near the
  // bottom so it doesn't cover the agent's prompt.
  let {
    rect,
  }: {
    rect: { x: number; y: number; w: number; h: number } | null;
  } = $props();
</script>

{#if rect}
  <div
    class="nudge"
    role="status"
    aria-live="polite"
    style="left:{rect.x}px;top:{rect.y}px;width:{rect.w}px;height:{rect.h}px"
  >
    <div class="card">
      <span class="msg">Handled — move along</span>
      <span class="keys"><kbd>Tab</kbd> next · <kbd>Esc</kbd> stay</span>
    </div>
  </div>
{/if}

<style>
  /* Confined to the focused pane's slot; pointer-events none so it never eats a
     click — it's a passive cue, and every key is handled at the App level. */
  .nudge {
    position: absolute;
    z-index: 30;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    pointer-events: none;
    padding-bottom: 18px;
  }
  .card {
    display: flex;
    align-items: center;
    gap: 12px;
    background: var(--color-surface-raised);
    border: 1px solid var(--color-border);
    border-radius: 8px;
    padding: 8px 14px;
    color: var(--color-text);
    font-family: var(--font-sans);
    font-size: var(--font-size-sm);
    line-height: 1.4;
    box-shadow: var(--shadow-lg);
  }
  .msg {
    font-weight: 600;
  }
  .keys {
    color: var(--color-text-muted);
  }
  kbd {
    background: var(--color-border-strong);
    color: var(--color-text);
    border-radius: 4px;
    padding: 1px 5px;
    font: inherit;
    font-weight: 700;
  }
</style>
