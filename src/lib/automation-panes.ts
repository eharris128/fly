// Placement helpers for automation agent-run tabs (U8, R9). Pure and
// unit-tested, mirroring layout.ts / workspaces.ts: App.svelte owns the live
// `$state`, the leaf/tab id minting, and the maps read at mount; this module
// only decides *where* an incoming agent run's tab should land.

import type { Workspace } from "./workspaces";

/**
 * The workspace an agent run's background tab should land in (R9): the origin
 * workspace named by the event if it still exists, else the first workspace
 * (the documented fallback — workspace ids are per-launch and in-memory, so an
 * `originWorkspaceHint` from a prior run, or from a since-closed workspace,
 * won't match and falls through here). `null` only when there are no
 * workspaces at all (nothing to place into yet).
 */
export function resolveTargetWorkspace(
  workspaces: Workspace[],
  originWorkspaceHint: string,
): string | null {
  if (workspaces.length === 0) return null;
  const match = workspaces.find((w) => w.id === originWorkspaceHint);
  return (match ?? workspaces[0]).id;
}
