// The command palette's command model + pure search (a U4 follow-up: the hotkey
// plan deferred the "searchable command palette — type-to-filter + Enter-to-run"
// to this). A palette command is anything runnable: every leader action (sourced
// straight from BINDINGS so it can't drift from the real chords — R3/KTD1) plus
// dynamic navigation built from live workspace/tab state. Kept pure and
// framework-free so it unit-tests without a DOM, mirroring layout.ts / keymap.ts.

import { BINDINGS, LEADER_KEY, formatLeader, type KeymapActions } from "./keymap";

export interface PaletteCommand {
  /** Stable id; also the keyed-list key in the overlay. */
  id: string;
  /** Primary, searchable label (the only field matched). */
  title: string;
  /** Secondary text shown right-aligned: a chord ("Ctrl-A c") or a category
   *  ("workspace" / "tab"). Not searched. */
  hint?: string;
  /** Execute the command. */
  run: () => void;
}

/**
 * One command per leader chord, derived from BINDINGS — the same array
 * `dispatch()` and the cheat-sheet render from, so the palette can never list a
 * stale action (R3/KTD1). The palette's own opener is skipped so it can't
 * invoke itself.
 */
export function actionCommands(
  actions: KeymapActions,
  leader: string,
): PaletteCommand[] {
  const lead = formatLeader(leader);
  return BINDINGS.filter((b) => b.action !== "openPalette").map((b) => {
    // keys[0] is the canonical chord key; cased to match how it's typed (X vs
    // x). The double-tap binding's key is the leader itself (U10/LEADER_KEY).
    const key =
      b.keys[0] === LEADER_KEY ? lead : b.upper ? b.keys[0].toUpperCase() : b.keys[0];
    return {
      id: `action:${b.action}`,
      title: b.label,
      hint: `${lead} ${key}`,
      run: actions[b.action],
    };
  });
}

/** The minimal workspace shape the nav builder needs — a structural subset of
 *  App's resolved sidebar view model, so it can be passed straight through. */
export interface NavWorkspace {
  id: string;
  name: string;
  tabs: { id: string; title: string }[];
}

/**
 * Dynamic "jump to …" commands: one per workspace and one per tab, built from
 * live state each time the palette opens. Selecting a workspace switches to it
 * (keeping its active tab); selecting a tab switches to that exact tab.
 */
export function navCommands(
  workspaces: NavWorkspace[],
  onSelectWorkspace: (wsId: string) => void,
  onSelectTab: (wsId: string, tabId: string) => void,
): PaletteCommand[] {
  const cmds: PaletteCommand[] = [];
  for (const w of workspaces) {
    cmds.push({
      id: `ws:${w.id}`,
      title: w.name,
      hint: "workspace",
      run: () => onSelectWorkspace(w.id),
    });
    for (const t of w.tabs) {
      cmds.push({
        id: `tab:${w.id}:${t.id}`,
        title: `${w.name} / ${t.title}`,
        hint: "tab",
        run: () => onSelectTab(w.id, t.id),
      });
    }
  }
  return cmds;
}

/**
 * Case-insensitive subsequence match: every character of `query` must appear in
 * `text` in order. Returns a score (lower = better — rewards earlier and more
 * contiguous matches) or `null` when there's no match. An empty query scores 0.
 */
export function fuzzyScore(text: string, query: string): number | null {
  if (query === "") return 0;
  const t = text.toLowerCase();
  const q = query.toLowerCase();
  let from = 0;
  let score = 0;
  let prev = -1;
  for (const c of q) {
    const at = t.indexOf(c, from);
    if (at === -1) return null;
    if (prev === -1) score += at; // leading gap → prefer prefix matches
    else score += at - prev - 1; // interior gap → prefer contiguous runs
    prev = at;
    from = at + 1;
  }
  return score;
}

/**
 * Rank commands against a query, best first. An empty/whitespace query
 * preserves source order; score ties also keep source order (stable sort), so
 * the list never jitters as you type.
 */
export function filterCommands(
  commands: PaletteCommand[],
  query: string,
): PaletteCommand[] {
  const q = query.trim();
  if (q === "") return commands;
  return commands
    .map((cmd, i) => ({ cmd, i, score: fuzzyScore(cmd.title, q) }))
    .filter(
      (m): m is { cmd: PaletteCommand; i: number; score: number } =>
        m.score !== null,
    )
    .sort((a, b) => a.score - b.score || a.i - b.i)
    .map((m) => m.cmd);
}
