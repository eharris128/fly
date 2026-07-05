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
    agentJumpTarget,
    firstRaised,
    type HomeWorkspaceGroup,
  } from "./home";
  import type { AutomationRow, LastStatus } from "./automations";
  import type { UsageSnapshot } from "../ipc";

  let {
    model,
    polledAt,
    usage,
    usageError,
    usageLoading,
    automations,
    automationsDegraded,
    automationsCorruptBak,
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
    /** Sorted, humanized automation rows (U10, R25). Read-only — no selection or
     *  jump, unlike the agent rows. */
    automations: AutomationRow[];
    /** Store health degraded (corrupt file or failing flush) — shows the R6
     *  warning row. */
    automationsDegraded: boolean;
    /** Where corrupt store bytes were preserved, for the R6 warning hint; null
     *  when degraded only by a failing flush. */
    automationsCorruptBak: string | null;
    /** Re-fetch the usage gauges on demand (the panel's refresh button). */
    onRefresh: () => void;
    onJump: (wsId: string, tabId: string, leafKey: string) => void;
    onClose: () => void;
  } = $props();

  // The last-run status word for a row: `"never run"` collapses to `"never"`;
  // every other value is already a single token that doubles as its CSS class.
  function statusWord(s: LastStatus): string {
    return s === "never run" ? "never" : s;
  }

  // Compact model/effort chip for an automation row (U9, R13). Scripts spend no
  // model → "—"; an agent with no pinned model → "Claude default"; otherwise the
  // model, with the effort appended when set ("opus · high").
  function modelLabel(a: AutomationRow): string {
    if (a.mode !== "agent") return "—";
    const model = a.model ?? "Claude default";
    return a.effort ? `${model} · ${a.effort}` : model;
  }

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
    } else if (e.key === "Enter") {
      // Enter jumps to the first agent needing attention (R7) — intercept before
      // the focused row button's native click. No-op when none need attention.
      e.preventDefault();
      const target = firstRaised(model);
      if (target) onJump(target.wsId, target.tabId, target.leafKey);
    } else if (!e.ctrlKey && !e.metaKey && !e.altKey && /^[0-9]$/.test(e.key)) {
      // Bare 0–9 → jump to that agent by its stable number (1–9 then 0 for the
      // tenth, R6/R8). The dashboard owns these digits, so consume any; an
      // out-of-range one resolves to null and just no-ops.
      e.preventDefault();
      const target = agentJumpTarget(model, Number(e.key));
      if (target) onJump(target.wsId, target.tabId, target.leafKey);
    }
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
    <span class="hint">Esc to close · ↑↓ to move · 1–9/0 to jump · Enter → first needing you</span>
  </header>

  {#if flatRows.length === 0}
    <div class="empty">
      <p>No Claude Code agents detected.</p>
      <p class="sub">Start one in a pane to see it here.</p>
    </div>
  {:else}
    <div class="groups">
      {#each model as ws (ws.wsId)}
        <section class="ws">
          <h2 class="ws-name">
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
                  class:attn={row.needsAttention}
                  role="option"
                  aria-selected={row.leafKey === selectedKey}
                  bind:this={rowEls[row.leafKey]}
                  onclick={() => activate(row.wsId, row.tabId, row.leafKey)}
                >
                  {#if row.num != null}
                    <kbd class="num">{row.num}</kbd>
                  {:else}
                    <span class="num"></span>
                  {/if}
                  {#if row.reason}
                    <span class="status reason-{row.reason}">{row.reason}</span>
                  {:else}
                    <span class="status {row.status}">{row.status}</span>
                  {/if}
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

  <!-- Automations panel (U10, R25): read-only, stacked below the agent list in
       the same left column. Static rows — no keyboard selection or jump. -->
  <section class="automations" aria-label="Automations">
    <header class="auto-head">
      <h2>Automations</h2>
      <span class="hint">read-only · manage with <code>fly automation</code></span>
    </header>

    {#if automationsDegraded}
      <p class="auto-warn">
        ⚠ Store corrupted · see
        <code>{automationsCorruptBak ?? "~/.local/share/fly/automations.json"}</code>
      </p>
    {/if}

    {#if automations.length === 0}
      <p class="auto-empty">
        No automations · Run <code>fly automation create --help</code> to get started
      </p>
    {:else}
      <ul class="auto-list">
        {#each automations as a (a.id)}
          <li class="auto-row" class:paused={a.paused} title={a.lastError ?? ""}>
            <span class="a-status s-{statusWord(a.lastStatus)}">{statusWord(a.lastStatus)}</span>
            <span class="a-name">{a.name}<span class="a-mode">{a.mode}</span>{#if a.retryOnInterrupt}<span class="a-retry" title="re-runs once if an app crash/restart interrupts it">retry</span>{/if}</span>
            <span class="a-meta">
              <span class="a-sched">{a.schedule}</span>
              <span class="a-model" title="launch model · effort">{modelLabel(a)}</span>
              <span class="a-next">{a.paused ? "paused" : `next ${a.nextRun}`}</span>
              {#if a.lastRun}<span class="a-last">· last {a.lastRun}</span>{/if}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>
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
    grid-template-columns: 28px 84px 64px 1fr;
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
  /* Raised agents stand out even when not selected (R5): an amber accent that
     echoes the in-pane attention ring. Defined before .selected so the active
     cursor's blue border wins when a row is both raised and selected. */
  .row.attn {
    border-color: #f5a623;
  }
  .row.selected {
    border-color: #4d7cff;
    background: #232a40;
  }
  /* Per-agent jump keycap (R4/R8). The empty placeholder span holds the column
     for un-numbered rows past ten so the grid stays aligned. */
  .num {
    min-width: 18px;
    height: 18px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0 4px;
    border-radius: 4px;
    background: #2a3350;
    color: #cdd3ea;
    font: inherit;
    font-size: 11px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
  }
  span.num {
    background: transparent;
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
  /* Reason-typed triage badge (R4): on a raised row the status slot shows *why*
     the agent needs you instead of the generic "waiting", so the row never reads
     a contradictory "waiting … finished". Distinct hue per reason; the label word
     itself carries the meaning, so the distinction is not color-only. (`error` is
     unfed in v1 but styled for completeness; `alert` is the automations watchdog
     reason (U12/R18) — only reachable on an agent row if a pane sends it via
     `fly notify`, since the alerts sink pane is not an agent-list row.) */
  .status.reason-question {
    color: #fbbf24;
  }
  .status.reason-permission {
    color: #fb923c;
  }
  .status.reason-alert {
    color: #2dd4bf;
  }
  .status.reason-finished {
    color: #818cf8;
  }
  .status.reason-error {
    color: #f87171;
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

  /* Automations panel — stacked below the agent list in the same left column,
     with a divider so the two regions read as distinct. Static text only. */
  .automations {
    max-width: 720px;
    margin-top: 28px;
    padding-top: 18px;
    border-top: 1px solid #262d44;
  }
  .auto-head {
    display: flex;
    align-items: baseline;
    gap: 12px;
    margin-bottom: 12px;
  }
  .auto-head h2 {
    font-size: 14px;
    font-weight: 600;
    margin: 0;
  }
  .auto-head .hint {
    font-size: 12px;
    color: #7b84a3;
  }
  .auto-head code,
  .auto-empty code,
  .auto-warn code {
    font-size: 11px;
    color: #8b93b2;
  }
  .auto-warn {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px;
    font-size: 12px;
    color: #f0a8a8;
    background: #2a1a1e;
    border: 1px solid #5a2a30;
    border-radius: 6px;
    padding: 8px 12px;
    margin: 0 0 12px;
  }
  .auto-warn code {
    color: #f0c0c0;
  }
  .auto-empty {
    font-size: 13px;
    color: #7b84a3;
    margin: 0;
  }
  .auto-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .auto-row {
    display: grid;
    grid-template-columns: 72px 1fr auto;
    align-items: baseline;
    gap: 12px;
    background: #1d2336;
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 8px 12px;
  }
  /* Paused rows recede — they have no upcoming occurrence. */
  .auto-row.paused {
    opacity: 0.72;
  }
  .a-status {
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .a-status.s-succeeded {
    color: #4ade80;
  }
  .a-status.s-failed {
    color: #f87171;
  }
  .a-status.s-running {
    color: #38bdf8;
  }
  .a-status.s-skipped {
    color: #fbbf24;
  }
  .a-status.s-never {
    color: #7b84a3;
  }
  .a-name {
    font-size: 13px;
    color: #e6e9f2;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .a-mode {
    margin-left: 8px;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: #8b93b2;
    background: #2a3350;
    border-radius: 4px;
    padding: 1px 5px;
  }
  /* Interrupt-resilience tag (interrupt-resilience U5): only shown when opted in,
     so it reads as an at-a-glance "this one survives a crash" marker. */
  .a-retry {
    margin-left: 6px;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: #9ecbff;
    background: #1f3350;
    border-radius: 4px;
    padding: 1px 5px;
  }
  .a-meta {
    display: flex;
    gap: 8px;
    font-size: 12px;
    color: #7b84a3;
    white-space: nowrap;
  }
  .a-next {
    color: #aeb6d4;
    font-variant-numeric: tabular-nums;
  }
  /* Launch model · effort chip (U9). Subtle — it is reference detail, not the
     row's headline. */
  .a-model {
    color: #8b93b2;
    font-variant-numeric: tabular-nums;
  }
</style>
