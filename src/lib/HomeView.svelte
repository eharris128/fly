<script lang="ts">
  // Agent dashboard home view (U7). Presentational: it renders the grouped
  // model from `home.ts` and owns only its keyboard selection + a 1s tick for
  // the live "working for" timer. All data + the never-unmount hide live in
  // App.svelte; jumping and closing are prop callbacks.
  import {
    formatDuration,
    formatTaskCount,
    formatResetTime,
    usageLimitLabel,
    workspaceJumpTarget,
    type HomeWorkspaceGroup,
  } from "./home";
  import type { UsageSnapshot } from "../ipc";

  let {
    model,
    polledAt,
    usage,
    usageError,
    usageLoading,
    onRefresh,
    onJump,
    onClose,
  }: {
    model: HomeWorkspaceGroup[];
    /** Wall-clock ms when `model`'s workingForMs values were measured, for the
     *  re-anchored local tick. */
    polledAt: number;
    /** Live `/usage` gauges, fetched on dashboard open; null until the first
     *  fetch resolves or after a failure (see `usageError`). */
    usage: UsageSnapshot | null;
    /** One-line reason the usage fetch failed (not signed in, offline, …). */
    usageError: string | null;
    usageLoading: boolean;
    /** Re-fetch the usage gauges on demand (the panel's refresh button). */
    onRefresh: () => void;
    onJump: (wsId: string, tabId: string, leafKey: string) => void;
    onClose: () => void;
  } = $props();

  // Selection is tracked by leaf key (stable across live updates), never index.
  let selectedKey = $state<string | null>(null);
  let now = $state(Date.now());
  let rowEls: Record<string, HTMLButtonElement | undefined> = {};

  // Flat row order (workspace → tab → row) for ↑/↓ navigation.
  const flatRows = $derived(model.flatMap((ws) => ws.tabs.flatMap((t) => t.rows)));

  // Keep selection valid: default to the first row, recover to the first if the
  // selected row disappears while the view is open (a pane exited). Stable
  // otherwise, so a row appearing/exiting never strands the cursor.
  $effect(() => {
    if (flatRows.length === 0) {
      selectedKey = null;
    } else if (
      selectedKey == null ||
      !flatRows.some((r) => r.leafKey === selectedKey)
    ) {
      selectedKey = flatRows[0].leafKey;
    }
  });

  // Move DOM focus to the selected row so Enter (native button click) and the
  // highlight stay in sync; runs on mount (first row) and every selection move.
  $effect(() => {
    if (selectedKey) rowEls[selectedKey]?.focus();
  });

  // Live tick: re-render elapsed timers each second. The component is mounted
  // only while the dashboard is open, so this is naturally gated.
  $effect(() => {
    const id = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(id);
  });

  function elapsedFor(workingForMs: number | null): number | null {
    if (workingForMs == null) return null;
    const offset = polledAt > 0 ? Math.max(0, now - polledAt) : 0;
    return workingForMs + offset;
  }

  function move(delta: number) {
    if (flatRows.length === 0) return;
    const idx = flatRows.findIndex((r) => r.leafKey === selectedKey);
    const base = idx < 0 ? 0 : idx;
    selectedKey = flatRows[(base + delta + flatRows.length) % flatRows.length].leafKey;
  }

  function activate(wsId: string, tabId: string, leafKey: string) {
    onJump(wsId, tabId, leafKey);
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || e.key === "j") {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp" || e.key === "k") {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (!e.ctrlKey && !e.metaKey && !e.altKey && /^[1-9]$/.test(e.key)) {
      // Bare 1–9 → jump to the first tab in the Nth displayed workspace, 1-based
      // to match the badges. The dashboard owns these digits, so consume any;
      // an out-of-range one resolves to null and just no-ops.
      e.preventDefault();
      const target = workspaceJumpTarget(model, Number(e.key));
      if (target) onJump(target.wsId, target.tabId, target.leafKey);
    }
    // Enter is handled by the focused row button's native click.
  }
</script>

<div class="dash">
  <div
    class="home"
    role="listbox"
    tabindex="-1"
    aria-label="Agent dashboard"
    onkeydown={onKeydown}
  >
  <header class="home-head">
    <h1>Agents</h1>
    <span class="hint">Esc to close · ↑↓ to move · Enter to jump · 1–9 workspace</span>
  </header>

  {#if flatRows.length === 0}
    <div class="empty">
      <p>No Claude Code agents detected.</p>
      <p class="sub">Start one in a pane to see it here.</p>
    </div>
  {:else}
    <div class="groups">
      {#each model as ws, i (ws.wsId)}
        <section class="ws">
          <h2 class="ws-name">
            {#if i < 9}<kbd class="ws-num">{i + 1}</kbd>{/if}
            <span class="ws-label">{ws.name}</span>
          </h2>
          {#each ws.tabs as tab (tab.tabId)}
            <div class="tab">
              <h3 class="tab-title">{tab.title}</h3>
              {#each tab.rows as row (row.leafKey)}
                {@const elapsed = elapsedFor(row.workingForMs)}
                <button
                  type="button"
                  class="row"
                  class:selected={row.leafKey === selectedKey}
                  role="option"
                  aria-selected={row.leafKey === selectedKey}
                  bind:this={rowEls[row.leafKey]}
                  onclick={() => activate(row.wsId, row.tabId, row.leafKey)}
                >
                  <span class="status {row.status}">{row.status}</span>
                  <span class="dur">
                    {#if row.status === "working" && elapsed != null}
                      {formatDuration(elapsed)}
                    {:else if row.status === "running"}
                      {formatTaskCount(row.liveTaskCount)}
                    {/if}
                  </span>
                  <span class="cwd">{row.cwd ?? ""}</span>
                </button>
              {/each}
            </div>
          {/each}
        </section>
      {/each}
    </div>
  {/if}
  </div>

  <aside class="usage-panel" aria-label="Plan usage">
    <header class="usage-head">
      <h2>Usage</h2>
      <div class="usage-actions">
        {#if usage?.plan}<span class="plan">{usage.plan}</span>{/if}
        <button
          type="button"
          class="refresh"
          class:spinning={usageLoading}
          onclick={onRefresh}
          disabled={usageLoading}
          title="Refresh usage"
          aria-label="Refresh usage"
        >↻</button>
      </div>
    </header>

    {#if usageError}
      <p class="usage-msg error">{usageError}</p>
    {:else if usage == null}
      <p class="usage-msg">{usageLoading ? "Loading…" : "—"}</p>
    {:else if usage.limits.length === 0}
      <p class="usage-msg">No active plan limits.</p>
    {:else}
      <ul class="limits">
        {#each usage.limits as lim (lim.kind + (lim.scopeLabel ?? ""))}
          {@const reset = formatResetTime(lim.resetsAt, now)}
          {@const pct = Math.min(100, Math.max(0, lim.percent))}
          <li class="limit" class:active={lim.isActive}>
            <div class="limit-top">
              <span class="limit-label">{usageLimitLabel(lim)}</span>
              <span class="limit-pct">{Math.round(lim.percent)}%</span>
            </div>
            <div class="bar" role="progressbar" aria-valuenow={Math.round(pct)}>
              <div class="bar-fill sev-{lim.severity ?? 'normal'}" style="width: {pct}%"></div>
            </div>
            {#if reset}<span class="limit-reset">{reset}</span>{/if}
          </li>
        {/each}
      </ul>
      <p class="usage-foot">Refreshed on open · from <code>/usage</code></p>
    {/if}
  </aside>
</div>

<style>
  /* Two columns: the agent list (left, grows) + the usage panel (right, fixed).
     Mirrors the sizing the .home root used to carry so it slots into App's
     layout identically. */
  .dash {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: row;
  }
  .home {
    flex: 1;
    min-width: 0;
    min-height: 0;
    overflow-y: auto;
    background: #161b2c;
    color: #e6e9f2;
    padding: 20px 24px;
    outline: none;
  }
  .usage-panel {
    flex: none;
    width: 300px;
    min-height: 0;
    overflow-y: auto;
    background: #12172480;
    border-left: 1px solid #262d44;
    color: #e6e9f2;
    padding: 20px 18px;
  }
  .usage-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 16px;
  }
  .usage-head h2 {
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #aeb6d4;
    margin: 0;
  }
  .usage-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .plan {
    font-size: 11px;
    font-weight: 600;
    text-transform: capitalize;
    color: #cdd3ea;
    background: #2a3350;
    border-radius: 4px;
    padding: 1px 7px;
  }
  /* Manual re-fetch — spins while a fetch is in flight, disabled meanwhile. */
  .refresh {
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    padding: 0;
    border: 1px solid #2a3350;
    border-radius: 5px;
    background: #1d2336;
    color: #aeb6d4;
    font-size: 13px;
    line-height: 1;
    cursor: pointer;
  }
  .refresh:hover:not(:disabled) {
    background: #232a40;
    color: #e6e9f2;
  }
  .refresh:disabled {
    cursor: default;
    opacity: 0.7;
  }
  .refresh.spinning {
    animation: refresh-spin 0.8s linear infinite;
  }
  @keyframes refresh-spin {
    to {
      transform: rotate(360deg);
    }
  }
  .usage-msg {
    font-size: 13px;
    color: #7b84a3;
    margin: 0;
  }
  .usage-msg.error {
    color: #f0a8a8;
  }
  .limits {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  .limit-top {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 6px;
  }
  .limit-label {
    font-size: 13px;
    color: #cdd3ea;
  }
  /* The currently-binding window stands out from the others. */
  .limit.active .limit-label {
    font-weight: 600;
    color: #e6e9f2;
  }
  .limit-pct {
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    color: #aeb6d4;
  }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: #2a3350;
    overflow: hidden;
  }
  .bar-fill {
    height: 100%;
    border-radius: 3px;
    background: #4d7cff;
    transition: width 0.3s ease;
  }
  /* Severity tints, escalating as a window fills (Claude's `severity`). */
  .bar-fill.sev-warning {
    background: #fbbf24;
  }
  .bar-fill.sev-critical {
    background: #f87171;
  }
  .limit-reset {
    display: block;
    margin-top: 5px;
    font-size: 11px;
    color: #7b84a3;
  }
  .usage-foot {
    margin: 20px 0 0;
    font-size: 11px;
    color: #5d667f;
  }
  .usage-foot code {
    font-size: 11px;
    color: #8b93b2;
  }
  .home-head {
    display: flex;
    align-items: baseline;
    gap: 12px;
    margin-bottom: 18px;
  }
  .home-head h1 {
    font-size: 18px;
    font-weight: 600;
    margin: 0;
  }
  .hint {
    font-size: 12px;
    color: #7b84a3;
  }
  .empty {
    margin-top: 64px;
    text-align: center;
    color: #7b84a3;
  }
  .empty .sub {
    font-size: 13px;
    margin-top: 4px;
  }
  .groups {
    display: flex;
    flex-direction: column;
    gap: 18px;
    max-width: 720px;
  }
  .ws-name {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    margin: 0 0 8px;
  }
  .ws-label {
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #8b93b2;
  }
  /* Keycap badge: the 0-based number that jumps to this workspace's first tab. */
  .ws-num {
    flex: none;
    min-width: 14px;
    padding: 1px 5px;
    border-radius: 4px;
    background: #2a3350;
    color: #cdd3ea;
    font: inherit;
    font-size: 11px;
    font-weight: 700;
    line-height: 1.45;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }
  .tab {
    margin-bottom: 10px;
  }
  .tab-title {
    font-size: 13px;
    font-weight: 500;
    color: #aeb6d4;
    margin: 0 0 4px;
  }
  .row {
    display: grid;
    grid-template-columns: 84px 64px 1fr;
    align-items: center;
    gap: 12px;
    width: 100%;
    text-align: left;
    background: #1d2336;
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 8px 12px;
    margin: 3px 0;
    color: inherit;
    font: inherit;
    cursor: pointer;
  }
  .row:hover {
    background: #232a40;
  }
  .row.selected {
    border-color: #4d7cff;
    background: #232a40;
  }
  .status {
    font-size: 12px;
    font-weight: 600;
    text-transform: capitalize;
  }
  .status.working {
    color: #4ade80;
  }
  .status.waiting {
    color: #fbbf24;
  }
  .status.idle {
    color: #7b84a3;
  }
  /* Busy-but-quiet: live background work, no output stretch. A cyan/teal distinct
     from working(green), waiting(amber), idle(gray). */
  .status.running {
    color: #38bdf8;
  }
  .dur {
    font-variant-numeric: tabular-nums;
    font-size: 13px;
    color: #cdd3ea;
  }
  .cwd {
    font-size: 12px;
    color: #7b84a3;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
