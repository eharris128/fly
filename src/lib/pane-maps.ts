// Pane-id bookkeeping helpers (audit-remediation U9/KTD9) — pure and
// framework-free like `layout.ts`, so both halves of the close-during-spawn
// fix are vitest-testable without mounting a component.

import type { PaneId } from "../ipc";

/// What a resolved `spawnPane` should do given whether the component was
/// destroyed while the spawn was in flight (KTD9): a pane that arrives after
/// its component's cleanup ran has no owner — `onDestroy` saw `paneId ===
/// null` and closed nothing — so the component must close the fresh pane
/// immediately and never announce it (`onSpawned` would register a dead leaf).
export function resolveSpawnRace(destroyed: boolean): {
  closeNow: boolean;
  announce: boolean;
} {
  return destroyed
    ? { closeNow: true, announce: false }
    : { closeNow: false, announce: true };
}

/// Prune the two pane-id maps to the live leaf set (KTD9): `paneIdByLeaf`
/// keeps only live leaves; `leafByPaneId` keeps only entries whose leaf is
/// live AND still maps back to that pane id (a reused pane id was already
/// overwritten to its new leaf — the stale reverse entry must not resurrect).
/// Called on every close path (pane, tab, workspace), same lifecycle as
/// notification pruning. Returns fresh objects; the inputs are not mutated.
export function prunePaneIdMaps(
  live: Set<string>,
  paneIdByLeaf: Record<string, PaneId>,
  leafByPaneId: Record<number, string>,
): {
  paneIdByLeaf: Record<string, PaneId>;
  leafByPaneId: Record<number, string>;
} {
  const forward: Record<string, PaneId> = {};
  for (const [leaf, id] of Object.entries(paneIdByLeaf)) {
    if (live.has(leaf)) forward[leaf] = id;
  }
  const reverse: Record<number, string> = {};
  for (const [idStr, leaf] of Object.entries(leafByPaneId)) {
    const id = Number(idStr);
    if (live.has(leaf) && forward[leaf] === id) reverse[id] = leaf;
  }
  return { paneIdByLeaf: forward, leafByPaneId: reverse };
}
