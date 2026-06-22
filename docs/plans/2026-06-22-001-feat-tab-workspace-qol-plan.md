---
title: "feat: Tab/workspace quality-of-life — digit tab nav, drag-reorder, cwd inheritance, clear-on-open"
date: 2026-06-22
type: feat
status: ready
depth: standard
origin: none (solo planning from direct feature requests)
---

# feat: Tab/workspace quality-of-life batch

## Summary

Four small, mostly independent quality-of-life features for `fly`, all **frontend-only**:

1. **Leader → tab number** — press the leader, then a digit `1`–`9`, to jump to the Nth tab in the *active* workspace.
2. **Drag-reorder workspaces** — drag a workspace up/down in the sidebar to reorder; the new order persists.
3. **New-tab cwd inheritance** — a new tab (leader chord *and* the sidebar "+ tab" button) opens in the current focused pane's working directory instead of `$HOME`.
4. **Clear-notification-on-tab-open** — viewing a tab removes its notifications from the panel and clears the unread badge + sidebar attention dot; a notification raised on a tab you're already viewing is never shown.

The underlying handlers mostly already exist (`selectTab`, the notification lifecycle, the cwd plumbing, array-order persistence), so each feature is a focused extension rather than new subsystem work. No Rust/backend changes are required — confirmed by the Feature-4 product decisions below.

---

## Problem Frame

`fly` is a Tauri v2 + Svelte 5 desktop terminal for AI coding agents (PTY panes, tabs + splits, workspaces, an attention/notification pipeline). Day-to-day use surfaced four friction points:

- Switching among several tabs requires the sidebar/palette or focus-cycling — there's no fast keyboard jump like tmux's `prefix <n>`.
- Workspaces can only be created/deleted, not reordered, so the sidebar order is whatever creation order happened to be.
- Every new tab opens in `$HOME`, forcing a `cd` back to wherever you were working.
- Opening a tab that an agent had flagged leaves its notification sitting in the panel as history; there's no "I've seen it, make it go away" on view.

These are independent and individually small; the value is in shipping them together as a coherent QoL batch.

---

## Requirements

- **R1 — Digit tab navigation.** With the leader pending, a digit `1`–`9` selects the Nth (1-based) tab in the **active workspace**. Out-of-range digits (`n > tab count`), `0`, and shifted/non-digit keys are consumed no-ops (never leak to the PTY, never select a workspace). The 10th+ tab has no chord (documented limitation). Works identically whether a pane or a non-terminal element holds focus.
- **R2 — Drag-reorder workspaces.** A workspace can be dragged up/down in the sidebar to a new position. The new order **persists across restart**. Reordering preserves the active workspace, its active tab, the focused pane, per-workspace mute, and the attention/unread rollups (they follow the moved row). Tabs are *not* reorderable in this batch.
- **R3 — New-tab cwd inheritance.** A new tab created via the leader chord **or** the sidebar "+ tab" button opens its pane in the working directory of the source focused pane, falling back to `$HOME` when that cwd is unknown, the pane has exited, or the directory no longer exists. For "+ tab" on a non-active workspace, the source is that workspace's active tab's focused pane.
- **R4 — Clear notifications on tab open.** Viewing a tab (via any path: click, digit, palette, workspace switch, attention jump, panel jump) **removes** its notifications from the panel and clears the tab/workspace unread badge and the sidebar attention dot. A notification raised on the **currently-viewed** tab is never added to the panel ("born-cleared"). The in-pane amber ring fades to "acknowledged" on view and clears on the first keystroke (existing backend behavior — unchanged). A restored session's initial active tab is **not** auto-cleared on launch.

---

## Key Technical Decisions

- **KTD1 — Digit chords live on `Keymap` as a dedicated callback, not in `BINDINGS`.** `BINDINGS`/`KeymapActions` are uniformly parameterless (`() => void`) and are consumed by `dispatch()`, the hotkey menu, and `palette.ts`'s `actionCommands` (`run: actions[b.action]`). A parameterized `selectTab(n)` member in `KeymapActions` would break that uniform type (a `(n: number) => void` is not assignable to `() => void`) and fail `pnpm check`, and nine digit rows would clutter the menu/palette (tab-jump is *already* in the palette via `navCommands`). So the digit handler is a separate `onSelectTab?: (n: number) => void` constructor arg on `Keymap`, dispatched from a digit branch in `Keymap.dispatch`. The hotkey menu gets **one** documentation row sourced from `keymap.ts` so it stays co-located with the dispatch logic. Digit handling lives in `Keymap` (the single shared, stateful instance) so the xterm key path and the window key path behave identically (per `src/lib/keymap.ts` and the shared-instance hazard noted in the research).
- **KTD2 — Pointer-drag reorder, not HTML5 native DnD.** The app runs on WebKitGTK, where native HTML5 DnD is flaky and a mid-drag re-render (e.g. an attention event re-deriving the sidebar) can abort the drag. There is no DnD precedent in the repo; there *is* a proven pointer-drag pattern (`startDrag` for split dividers in `src/App.svelte`). Mirror it: `pointerdown` on a workspace row → `pointermove`/`pointerup` listeners on `window`, torn down together; compute an insertion index from pointer Y; on `pointerup` fire a new `onReorderWorkspace(from, to)` callback. The Sidebar stays presentational (computes index, fires callback); `App.svelte` owns the array mutation.
- **KTD3 — Reorder mutates the `workspaces` `$state` array; persistence is free.** Workspace order is serialized purely by array position (`persist()` walks `workspaces` in order; `restore()` rebuilds in order with fresh ephemeral ids). Reassigning `workspaces = reorderWorkspaces(...)` re-fires the 800 ms debounced save `$effect` automatically. `activeWorkspaceId` and `mutedWorkspaces` are keyed by workspace **id**, so they survive reordering untouched — no new persisted field, no `set_pane_workspace` re-push (pane→workspace mapping is id-keyed in the backend).
- **KTD4 — New-tab cwd is queried fresh at creation, with synchronous source capture.** Seed from a fresh `paneCwd(srcPid)` read (so a just-issued `cd` is honored), falling back to the polled `cwdByLeaf` cache then `null`/`$HOME`. `newTab` becomes `async`; the **source leaf key + paneId are captured synchronously before the `await`** so concurrent "+ tab" requests can't cross-wire. The new tab is added and its `cwdByLeaf[newLeafKey]` seeded in the same synchronous chunk after the await, so the Terminal — which reads its `cwd` prop **once at mount** — sees the value before mounting.
- **KTD5 — "Clear on view" removes entries; it does not mark-read.** A new pure `clearForLeaves(list, leafKeys)` (identity-stable, mirroring `markReadForLeaves`) **removes** a tab's notifications. The unread badge and panel both derive from `notifications`, so they clear together; clearing reaches disk via the existing `notifications` save `$effect`. The sidebar attention dot (which checks `attentionByLeaf[k] === "raised"`) clears via the **existing** backend ack (`setVisiblePanes` drives Raised→Acknowledged on visibility). The in-pane ring stays backend-authoritative and is **not** force-cleared — per the product decision, it fades on view and clears on first keystroke. No backend change.
- **KTD6 — Born-cleared at ingestion, restore-safe.** A live raise whose leaf is currently in the visible set (the active tab) is dropped in `onNotificationAdded` rather than appended. Restored notifications enter via a separate path (`coerceNotifications` at restore), so the initial active tab's unread history is preserved on launch (consistent with the existing "no auto-read on launch" rule).

---

## High-Level Technical Design

Feature 4 is the only piece touching multiple signals with different clearing mechanisms. The matrix below is the authoritative map of *what* clears a tab's "needs attention" indication on open and *which* of those this plan changes — everything else derives or is already handled by the backend ack.

| Signal | Source of truth | How it clears on tab-open | Change in this plan |
|---|---|---|---|
| Notification panel entry | `notifications[]` (frontend list) | removed by new `clearForLeaves` | **NEW** |
| Unread badge (tab / workspace / control-bar) | `unreadByLeaf(notifications)` (`$derived`) | follows entry removal automatically | none — derives |
| Sidebar attention dot | `attentionByLeaf[k] === "raised"` | existing backend ack drives Raised→Acknowledged; dot only shows for `"raised"` | none — existing |
| In-pane amber ring | Terminal's own `pane://attention` state | backend ack fades Raised→Acknowledged on view; → Idle on first keystroke | none — **per product decision** |
| Raise on the already-visible tab | ingestion (`onNotificationAdded`) | dropped when the leaf is currently visible ("born-cleared") | **NEW** |

The two **NEW** rows are the whole of Feature 4's frontend surface; the three "none" rows are why no backend work is needed.

---

## Implementation Units

### U1. Leader → tab number (digit chords)

- **Goal:** With the leader pending, `1`–`9` selects the Nth tab in the active workspace.
- **Requirements:** R1.
- **Dependencies:** none. (Interacts with U5 — the `selectTab` path clears notifications once U5 lands — but is implementable against the current mark-read behavior.)
- **Files:**
  - `src/lib/keymap.ts` — add `onSelectTab?: (n: number) => void` constructor arg to `Keymap`; add a digit branch in `dispatch()`; export a small constant describing the chord for the menu.
  - `src/App.svelte` — construct `Keymap` with an `onSelectTab` callback (the constructor parameter name from KTD1; pass a local function, e.g. `selectTabByIndex(n)`) that resolves `activeWorkspace.tabs[n - 1]` and calls the existing `selectTab(activeWorkspaceId, tab.id)` (no-op if absent).
  - `src/lib/HotkeyMenu.svelte` — render one documentation row for the digit chord, worded to convey the range (e.g. `1–9  Select tab N (no-op past tab count)`) from the exported constant. The out-of-range/`0` silent no-op is intentional and consistent with the existing unbound-chord behavior; the row text sets that expectation.
  - `src/lib/keymap.test.ts` — tests (see below); extend the `Keymap` constructions to pass the new callback.
- **Approach:** In `dispatch()`, after lowercasing, branch first on `/^[1-9]$/.test(e.key)` → call `this.onSelectTab?.(Number(e.key))` and return. Digits are not in `BINDINGS`, so the existing `BINDINGS.find` path is untouched and `leader 0` falls through to "no match → consumed no-op". Shifted digits arrive as `!@#…` (not `1`–`9`) and numpad-with-numlock-off arrives as `End/ArrowDown/…`, so both naturally fall through. The App callback maps 1-based `n` → `tabs[n-1]`; out-of-range is a silent no-op. `selectTab` on the already-active tab is idempotent.
- **Patterns to follow:** the `upper`-flag precedence and the modifier-swallow logic already in `Keymap` (`src/lib/keymap.ts`); the existing `keymapActions` wiring in `src/App.svelte`.
- **Technical design (directional):** keep the digit branch *inside* `Keymap` (not in `App.svelte`'s `onWindowKeydown`) so the chord works from both the xterm handler and the window handler against the one shared `leaderPending` state — a leader pressed in one path and a digit arriving in the other must complete the same chord exactly once.
- **Test scenarios** (`src/lib/keymap.test.ts`):
  - Covers R1. Leader then `3` with ≥3 tabs → `onSelectTab(3)` fires exactly once; the event is consumed (`handle` returns `true`).
  - Leader then `1` → `onSelectTab(1)` (asserts 1-based mapping, not `0`).
  - Leader then `0` → no `onSelectTab` call; event consumed (not leaked to PTY).
  - Leader then `9` with 3 tabs → `onSelectTab(9)` fires, but the App-level resolver yields no tab → no `selectTab`. (Test the App resolver separately or assert the callback is invoked and the index is out of range; keep the keymap test to "callback fired with 9".)
  - A bare digit `5` with **no** pending leader → not consumed (`handle` returns `false`), passes through to the PTY.
  - Leader then `!` (Shift+1) → no `onSelectTab`; consumed no-op.
  - Leader pressed, then a focus change, then `2` (same shared `Keymap` instance) → `onSelectTab(2)` fires once (chord survives the focus change).
  - Digit never triggers a workspace action (assert no other action spy fires).

### U2. `reorderWorkspaces` pure helper + insertion-index math

- **Goal:** Pure, DOM-free helpers for the reorder model and the drop-position math.
- **Requirements:** R2.
- **Dependencies:** none.
- **Files:**
  - `src/lib/workspaces.ts` — add `reorderWorkspaces(workspaces, fromIndex, toIndex): Workspace[]` and `insertionIndex(pointerY, rowMidpoints, draggedIndex): number` (both pure; `insertionIndex` takes plain numbers so the file stays DOM-free per its header contract).
  - `src/lib/workspaces.test.ts` — tests.
- **Approach:** `reorderWorkspaces` removes at `fromIndex`, inserts at the adjusted `toIndex`, returns a **new** array — but returns the **original array reference unchanged** when `fromIndex === toIndex` or indices are out of range (identity-stable, to avoid a spurious debounced save). `insertionIndex` maps a pointer Y against the row midpoints to a target index (insert-before when above a midpoint, after when below), excluding the dragged row's own slot.
- **Patterns to follow:** the pure, new-array-returning, id-factory style of `closeTabIn`/`deleteWorkspaceFrom`; the identity-stable return convention in `notifications.ts` (`return changed ? next : list`).
- **Test scenarios** (`src/lib/workspaces.test.ts`):
  - Covers R2. Move index 0 → 2 in `[A,B,C,D]` → `[B,C,A,D]` (drag down, index-shift correct).
  - Move index 3 → 1 in `[A,B,C,D]` → `[A,D,B,C]` (drag up).
  - Move to head (`x → 0`) and to tail (`x → len-1`) produce the expected end positions.
  - `fromIndex === toIndex` → returns the **same array reference** (`expect(result).toBe(input)`).
  - Out-of-range `fromIndex`/`toIndex` → same reference, no throw.
  - Single-element array → same reference.
  - `insertionIndex`: pointer above the first midpoint → 0; below the last → `len`(-adjusted); over the dragged row's own band → its current index (no-op); pointer between rows resolves to the gap index. (Name the midpoints and Y explicitly per case.)

### U3. Sidebar pointer-drag reorder wiring

- **Goal:** Drag a workspace row up/down to reorder, using U2's helpers.
- **Requirements:** R2.
- **Dependencies:** U2.
- **Files:**
  - `src/lib/Sidebar.svelte` — add a drag affordance on the workspace row (`.ws-head`), pointer handlers, a drag-hover insertion indicator (local component `$state`, like `folded`), and a new `onReorderWorkspace: (fromIndex: number, toIndex: number) => void` prop.
  - `src/App.svelte` — pass `onReorderWorkspace={(from, to) => { workspaces = reorderWorkspaces(workspaces, from, to); }}`.
- **Approach:** Mirror `startDrag` in `src/App.svelte`: `pointerdown` on a row's drag handle → `ev.preventDefault()`, record the dragged index, attach `pointermove`/`pointerup` to `window`. On move, compute `insertionIndex(e.clientY, rowMidpoints, draggedIndex)` (midpoints from each row's `getBoundingClientRect()`) and render an indicator. On `pointerup`, fire `onReorderWorkspace(from, target)` (the helper no-ops if equal) and tear down listeners. `Escape` or `pointercancel`/drop-outside cancels: clear the indicator, fire nothing. Keep existing row click/keyboard select/activate working (the drag handle or a movement threshold distinguishes a drag from a click). The active workspace, mute, and rollups follow automatically because they're id-keyed / `$derived` (KTD3). Single workspace → no drag targets, inert.
- **Interaction spec (directional — confirm/adjust at implementation):** a grip affordance (e.g. a `⠿` handle) appears on `.ws-head` on hover with `cursor: grab` and is the `pointerdown` initiator, so a drag is unambiguous vs. a row click; alternatively gate drag-start on a small movement threshold (~4 px) and switch the cursor on drag-start. The insertion indicator is a 2 px accent line (`#4da3ff`) at the computed gap with the dragged row at reduced opacity (~0.4) while in flight. On cancel (`Escape`/`pointercancel`/drop-outside) the indicator is removed synchronously and rows return to default with no animation (the 200 px sidebar is small). Dropping over the `.footer` or below the last row resolves to insert-at-last (the `insertionIndex` `len` case); the footer never takes a workspace as an action.
- **Patterns to follow:** `startDrag` (`src/App.svelte`) for the window-listener teardown discipline; the `e.stopPropagation()` on nested sidebar buttons so a row action doesn't double-fire row select; the `suppressBlur` rename guard if the drag handle sits near the inline-rename field.
- **Test scenarios:**
  - Automated (the pure core is in U2's `insertionIndex`/`reorderWorkspaces` tests — those cover the index correctness this unit depends on).
  - Manual verification (no Svelte component test harness exists in this repo — confirmed): drag a workspace down past one neighbor and release → order updates and the indicator clears. Drag and press `Escape` → order unchanged, indicator cleared. Drag a workspace that has a **raised agent + non-zero unread badge** → both indicators land on the moved row, not the old slot. Reorder a **muted** workspace → it stays muted. Quit within ~800 ms of a reorder and relaunch → new order persisted (close-flush). Reorder while an attention event arrives mid-drag → drag completes/applies without aborting. A lone workspace cannot be dragged anywhere.

### U4. New-tab cwd inheritance

- **Goal:** New tabs (both entry points) open in the source focused pane's cwd.
- **Requirements:** R3.
- **Dependencies:** none.
- **Files:**
  - `src/App.svelte` — make `newTab(wsId)` `async`; resolve the source leaf, query cwd, seed `cwdByLeaf` for the new leaf, then add the tab and set active.
  - `src/lib/workspaces.ts` — add `sourceLeafForNewTab(workspaces, wsId): string | null` (the target workspace's active tab's `focusedLeafKey`), pure.
  - `src/lib/workspaces.test.ts` — tests for `sourceLeafForNewTab`.
  - `src/lib/Terminal.svelte` — no change (already consumes the `cwd` prop; `App.svelte` already passes `cwd={cwdByLeaf[p.key] ?? null}`).
- **Approach (ordered, to respect the mount-once timing):**
  1. Synchronously compute `srcKey = sourceLeafForNewTab(workspaces, wsId)` (may be `null`) and `srcPid = srcKey != null ? paneIdByLeaf[srcKey] : null` **before** any await (avoids cross-wiring on concurrent calls; `paneIdByLeaf` is non-`$state`, read it synchronously).
  2. `const cwd = (srcPid != null ? await paneCwd(srcPid) : null) ?? (srcKey != null ? cwdByLeaf[srcKey] : null) ?? null;`
  3. `const t = makeTab(); const newKey = leaves(t.tree)[0].key;`
  4. In one synchronous block, **after re-checking the target still exists** (`workspaces.some(w => w.id === wsId)` — it may have been deleted during the `await`): `cwdByLeaf = { ...cwdByLeaf, [newKey]: cwd }; workspaces = workspaces.map(/* add t, set activeTabId */); activeWorkspaceId = wsId;` — so the new Terminal mounts on the next tick with `cwdByLeaf[newKey]` already set. If the target workspace vanished during the await, drop the new tab and leave `activeWorkspaceId` untouched (never point it at a deleted workspace, which would render a blank layout).
  - Fallbacks: cold cache / exited pane / no source → `null` → Terminal falls back to `$HOME`. A since-deleted or un-enterable inherited dir → portable-pty falls back to `$HOME` (existing behavior, `src/lib/Terminal.svelte` comment).
- **Patterns to follow:** `refreshCwds` / `persist`'s fresh `paneCwd` reads; the spread-reassign convention for `cwdByLeaf`; the non-`$state` nature of `paneIdByLeaf` (read it synchronously — don't expect an effect to track it).
- **Test scenarios:**
  - Automated (`src/lib/workspaces.test.ts`): `sourceLeafForNewTab` returns the active tab's `focusedLeafKey` for the target workspace (Covers R3 for the non-active-workspace case — the active workspace is *not* consulted); returns `null` when the workspace has no resolvable focused leaf; targeting a non-active workspace returns *that* workspace's focused leaf, not the globally-active one.
  - Manual verification (async spawn cwd has no unit harness): `cd /tmp` then `leader c` → new tab's shell starts in `/tmp`. "+ tab" on the active workspace → inherits the focused pane's cwd. "+ tab" on a *different* workspace → inherits that workspace's active tab's cwd, and active switches to it. New tab off a just-spawned (un-polled) pane → falls back to `$HOME`, no error. New tab off an exited pane → last-known cwd or `$HOME`, no error.

### U5. Clear notifications on tab open (+ born-cleared)

- **Goal:** Viewing a tab removes its notifications (panel + badge + sidebar dot); a raise on the visible tab is born-cleared; the ring is left to the existing backend ack.
- **Requirements:** R4.
- **Dependencies:** none. (U1's digit path and the panel/jump paths all route through the shared clear, so they inherit this behavior automatically.)
- **Files:**
  - `src/lib/notifications.ts` — add `clearForLeaves(list, leafKeys): Notification[]` (remove entries whose `leafKey` is in the set; identity-stable when nothing matches).
  - `src/App.svelte` — repurpose `markActiveTabRead()` → `clearActiveTabNotifications()` using `clearForLeaves` (call sites: `selectTab`, `focusPane`, `selectWorkspace`, `shiftWorkspace`); add the born-cleared guard in `onNotificationAdded`; in `jumpNewestUnread`/`onPanelJump`, **replace** the single-id `markRead` with a single-id **clear** (`clearNotifications([id])`) rather than dropping it (see closed-pane note in Approach).
  - `src/lib/NotificationPanel.svelte` — update the empty-state copy to reflect the new unviewed-only semantics (e.g. "No pending notifications — opening a tab clears them"), since the panel is no longer a persistent history log.
  - `src/lib/notifications.test.ts` — tests for `clearForLeaves`.
- **Approach:** `clearForLeaves` mirrors `markReadForLeaves` but filters entries out, returning the original list reference when no entry matched (avoids spurious persist/derive). `clearActiveTabNotifications` clears all of `leaves(activeTab.tree)` keys. Born-cleared: in `onNotificationAdded`, after resolving `paneId → leafKey`, if that leaf is in the current `visibleLeafKeys`, return without appending. Restore is unaffected (it doesn't flow through `onNotificationAdded`), so the initial tab keeps its unread history. The ring/dot need no code here — `pushVisiblePanes` already fires on the visibility `$effect` and the backend ack handles Raised→Acknowledged. **Closed-pane jump path:** `jumpNewestUnread`/`onPanelJump` only clear via `focusPane`, which is gated on `loc` resolving. For an entry whose pane no longer resolves to a live tab (`loc === null`), `focusPane`/`clearActiveTabNotifications` never runs, so the single-id `clearNotifications([id])` is the *only* dismissal — keep it (don't drop it), or `leader U` sticks on that entry since `newestUnread` keeps returning it. **Ring divergence:** the instant-clear of badge/dot while the in-pane ring only fades is intentional (the ring dims to the already-distinct `.acknowledged` style in `Terminal.svelte`, then clears on first keystroke); the distinct acknowledged styling keeps the faded ring from reading as a fresh raise.
- **Patterns to follow:** `markReadForLeaves` shape and the identity-stable return; the `leafKey`-keyed durability of the notification model; the existing `markActiveTabRead` call-site set.
- **Test scenarios** (`src/lib/notifications.test.ts`):
  - Covers R4. `clearForLeaves(list, [k1])` removes all entries for `k1` and leaves entries for other leaves intact.
  - Clearing a multi-leaf tab removes entries for all of its leaves, none of others.
  - `clearForLeaves` with leaves that have no entries → returns the **same array reference** (`toBe`), no churn.
  - After `clearForLeaves`, `unreadByLeaf` for the cleared leaves is 0 (badge derivation follows removal).
  - `clearForLeaves` is null-safe when a leaf no longer resolves to a live entry (already-pruned).
  - Closed-pane jump fallback: with a `loc === null` (closed/exited pane) unread entry, the single-id `clearNotifications([id])` removes it so `newestUnread` advances past it on the next `leader U` (no stuck-jump).
  - Born-cleared (logic-level): a notification ingested for a leaf in the visible set is not appended; the same notification for a non-visible leaf **is** appended. (Test the guard predicate directly if extracted, else cover via an `App`-level manual check.)
  - Manual verification: open a tab with a pending notification → panel row gone, tab + workspace badges gone, sidebar dot gone, in-pane ring faded (not bright). Sit on a tab and have an agent raise → nothing appears in the panel. Restart with unread history on the initial tab → it is still shown on launch (not auto-cleared). `leader U` / panel-jump into a tab → removes that tab's entries identically to a click.

---

## Scope Boundaries

**In scope:** the four features exactly as specified in R1–R4, frontend-only.

### Deferred to Follow-Up Work
- **Force the in-pane ring fully to Idle on tab-open (no keystroke).** Per the product decision, the ring fades on view and clears on first keystroke. Making it vanish on open would need a new backend `clear_attention(paneIds)` command + a new Idle transition in the attention state machine (`src-tauri/src/state/`) + Rust tests. Deferred.
- **Keyboard / touch workspace reordering.** v1 reorder is pointer/mouse only; rows stay keyboard-selectable. An `Alt+↑/↓` reorder is a later add.
- **Reordering tabs within a workspace.** Only workspaces reorder in this batch.
- **Digit chords for the 10th+ tab.** `1`–`9` only; tabs beyond 9 are reachable via the sidebar/palette.
- **Autoscroll while dragging a long workspace list.** If the workspace list ever exceeds the viewport, dragging to an off-screen slot would want autoscroll — out of scope unless it bites.

---

## Open Questions & Risks

- **Debounce vs. durability (accepted).** A clear or reorder followed by a crash within the 800 ms save window reverts on next launch (entries reappear / order resets). This matches the existing persistence model and the normal-quit close-flush covers clean exits — accepted, not worth a synchronous flush.
- **`newTab` becoming async.** Both callers (leader chord, sidebar "+ tab") are fire-and-forget, so no caller depends on synchronous creation — verified against current call sites. Risk is low; the sub-frame delay before the tab appears is imperceptible.
- **Born-cleared dropping something wanted (low).** The guard only drops a raise when its leaf is the *currently-visible* active tab — i.e. you are looking at it. Mitigated by scope; restore path is separate so missed-last-session notifications still surface.
- **Identity-stability regressions.** New pure helpers (`reorderWorkspaces`, `clearForLeaves`) must return the original reference on no-op or they will thrash the `$derived` view-models and the debounced `persist()`. Enforced by the `toBe` assertions in U2/U5.
- **Notification panel becomes unviewed-only (accepted).** Because viewing a tab now *removes* its notifications (not mark-read), the panel is no longer a persistent "what did agents do while I was away" history/audit log across tabs you've already visited — that record is intentionally not retained (the owner chose remove-over-keep-history). A side effect: the panel's `read` state and "Mark all read" become near-vestigial — only restore and the explicit "Mark all read" button still produce kept-read entries. Acceptable for v1; revisit if a durable activity log is later wanted.
- **Async `newTab` target-vanish (guarded).** Making `newTab` async opens a window where the target workspace could be deleted during the `await paneCwd`. U4 step 4 guards this (re-check `workspaces.some(w => w.id === wsId)` before mutating; bail if gone) so `activeWorkspaceId` is never left pointing at a deleted workspace (which would blank the layout).

---

## Sources & Research

No external research was warranted — all four features are internal UI/local-pattern work with strong existing patterns and no external contract surfaces. Grounding came from local analysis of the codebase:

- Keymap model and the `BINDINGS`/menu/palette anti-drift invariant: `src/lib/keymap.ts`, `src/lib/palette.ts`, `src/lib/HotkeyMenu.svelte`, `src/lib/CommandPalette.svelte`.
- Workspace model + array-order persistence: `src/lib/workspaces.ts`, `src/lib/serialize.ts`, `persist()`/`restore()` in `src/App.svelte`.
- cwd plumbing: `cwdByLeaf`/`refreshCwds` in `src/App.svelte`, `src/lib/Terminal.svelte`, `paneCwd`/`spawnPane` in `src/ipc.ts`.
- Notification/attention pipeline: `src/lib/notifications.ts`, `attentionByLeaf`/`onAttention`/`pushVisiblePanes` in `src/App.svelte`, `src/lib/Terminal.svelte` (`pane://attention`), and the backend ack (`set_visible_panes` → Raised→Acknowledged, never Idle).
- Svelte 5 reactivity conventions (reassign Set/Map/object `$state`; identity-stable pure helpers; `paneIdByLeaf` deliberately non-`$state` and re-pushed on `onSpawned`) — carried into KTD3/KTD4/KTD5.
- Test conventions: vitest pure-module tests in `src/lib/*.test.ts`; run with `pnpm test:unit`, `pnpm vitest run src/lib/<file>.test.ts`, `pnpm check` for types. No Svelte component test harness exists — pointer/async-spawn wiring is verified manually.
