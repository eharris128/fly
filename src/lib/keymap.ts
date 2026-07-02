// Keyboard model (U6): a configurable leader key gates app actions; everything
// else passes straight through to the PTY (R6). tmux-style — press the leader
// (default Ctrl-A), then a command key.

export interface KeymapActions {
  newTab: () => void;
  splitHorizontal: () => void;
  splitVertical: () => void;
  closePane: () => void;
  closeTab: () => void;
  focusLeft: () => void;
  focusRight: () => void;
  focusUp: () => void;
  focusDown: () => void;
  cycleAttention: () => void;
  jumpNewestUnread: () => void;
  openNotifications: () => void;
  toggleMute: () => void;
  openMenu: () => void;
  openPalette: () => void;
  toggleSidebar: () => void;
  toggleHome: () => void;
  newWorkspace: () => void;
  closeWorkspace: () => void;
  prevWorkspace: () => void;
  nextWorkspace: () => void;
  renameTab: () => void;
  handoffQuick: () => void;
  handoffGuided: () => void;
}

/**
 * A leader chord → action binding. `keys` are matched against
 * `e.key.toLowerCase()`, so punctuation arrives as its produced character
 * (`|`, `?`, `_`) and CapsLock never changes which letter matches. The single
 * exception is `upper`: when set, the binding only fires when the *literal*
 * `e.key` is the uppercase form (so `leader X` = close tab is distinct from
 * `leader x` = close pane). This is the one chord→action map; both `dispatch()`
 * and the hotkey menu (U4) render from it, so they can never drift (R3/KTD1).
 */
export interface Binding {
  keys: string[];
  upper?: boolean;
  label: string;
  action: keyof KeymapActions;
}

export const BINDINGS: Binding[] = [
  { keys: ["c"], label: "New tab", action: "newTab" },
  { keys: ["|", "\\"], label: "Split right", action: "splitHorizontal" },
  { keys: ["-", "_"], label: "Split down", action: "splitVertical" },
  { keys: ["x"], label: "Close pane", action: "closePane" },
  { keys: ["x"], upper: true, label: "Close tab", action: "closeTab" },
  { keys: ["h", "arrowleft"], label: "Focus left", action: "focusLeft" },
  { keys: ["l", "arrowright"], label: "Focus right", action: "focusRight" },
  { keys: ["k", "arrowup"], label: "Focus up", action: "focusUp" },
  { keys: ["j", "arrowdown"], label: "Focus down", action: "focusDown" },
  { keys: ["u"], label: "Cycle attention", action: "cycleAttention" },
  // Uppercase U is distinct from lowercase u (cycle attention) via `upper`,
  // exactly like x / X: it jumps within the notification history, not the
  // live-raised panes.
  { keys: ["u"], upper: true, label: "Jump to newest unread", action: "jumpNewestUnread" },
  { keys: ["n"], label: "Notifications", action: "openNotifications" },
  { keys: ["m"], label: "Toggle mute", action: "toggleMute" },
  { keys: ["r"], label: "Rename tab", action: "renameTab" },
  // Session handoff (U2, R1/R2, docs/plans/2026-07-02-001-feat-session-handoff-
  // plan.md): f and F are distinct via `upper`, exactly like x / X. Lowercase f
  // is the quick handoff (fresh agent in a split, stock pickup prompt sent
  // immediately); uppercase F is the guided handoff (bare agent, U3 pre-types
  // the prompt unsent so direction can be appended). Cheat-sheet + palette pick
  // both up automatically from this array (R2).
  { keys: ["f"], label: "Handoff (quick)", action: "handoffQuick" },
  { keys: ["f"], upper: true, label: "Handoff (guided)", action: "handoffGuided" },
  { keys: ["w"], label: "New workspace", action: "newWorkspace" },
  // Uppercase W is distinct from lowercase w (new workspace) via `upper`,
  // exactly like x / X (pane vs tab): it deletes the active workspace.
  { keys: ["w"], upper: true, label: "Close workspace", action: "closeWorkspace" },
  { keys: ["["], label: "Previous workspace", action: "prevWorkspace" },
  { keys: ["]"], label: "Next workspace", action: "nextWorkspace" },
  { keys: ["b"], label: "Toggle sidebar", action: "toggleSidebar" },
  { keys: ["d"], label: "Dashboard (home)", action: "toggleHome" },
  { keys: ["p"], label: "Command palette", action: "openPalette" },
  { keys: ["?"], label: "Hotkey menu", action: "openMenu" },
];

/**
 * The digit tab-switch chord (U1): leader then `1`–`9` selects the Nth tab in
 * the active workspace. It is intentionally NOT a `BINDINGS` entry — its action
 * is parameterized (the digit picks the tab), which doesn't fit the uniform
 * `() => void` action shape, and nine rows would clutter the menu/palette
 * (tab-jump is already reachable by name in the palette). Dispatch handles the
 * digits directly via the `onSelectTab` callback; this constant exists so the
 * hotkey menu documents the chord from the same module (no drift). KTD1.
 */
export const DIGIT_CHORD = {
  key: "1–9",
  label: "Select tab N (no-op past tab count)",
};

/**
 * Turn a leader spec (`"ctrl+a"`, `"super+space"`) into a display string for
 * the hotkey menu: `"Ctrl-A"`, `"Super-Space"` (R6). Known modifier/key names
 * are title-cased to their conventional forms; anything else is upper-cased
 * (single char) or capitalised (word).
 */
export function formatLeader(spec: string): string {
  const names: Record<string, string> = {
    ctrl: "Ctrl",
    super: "Super",
    meta: "Super",
    cmd: "Super",
    alt: "Alt",
    shift: "Shift",
    space: "Space",
  };
  return spec
    .toLowerCase()
    .split("+")
    .map((p) => p.trim())
    .filter((p) => p.length > 0)
    .map(
      (p) =>
        names[p] ??
        (p.length === 1 ? p.toUpperCase() : p[0].toUpperCase() + p.slice(1)),
    )
    .join("-");
}

/** Bare modifier keys, which fire their own keydown before the modified key. */
function isModifierKey(key: string): boolean {
  return (
    key === "Shift" || key === "Control" || key === "Alt" || key === "Meta"
  );
}

/** Build a matcher for a leader spec like "ctrl+a" or "super+space". */
export function parseLeader(spec: string): (e: KeyboardEvent) => boolean {
  const parts = spec.toLowerCase().split("+").map((p) => p.trim());
  const raw = parts.pop() ?? "a";
  // KeyboardEvent.key uses " " for the spacebar, not "space".
  const key = raw === "space" ? " " : raw;
  const ctrl = parts.includes("ctrl");
  const meta = parts.includes("super") || parts.includes("meta") || parts.includes("cmd");
  const alt = parts.includes("alt");
  const shift = parts.includes("shift");
  return (e) =>
    e.key.toLowerCase() === key &&
    e.ctrlKey === ctrl &&
    e.metaKey === meta &&
    e.altKey === alt &&
    e.shiftKey === shift;
}

export class Keymap {
  private leaderPending = false;
  private matchLeader: (e: KeyboardEvent) => boolean;

  constructor(
    leaderKey: string,
    private actions: KeymapActions,
    // Parameterized digit chord (leader 1–9 → select tab N). Kept separate from
    // the uniform, parameterless `KeymapActions` so BINDINGS/dispatch/palette
    // stay type-clean — a `(n: number) => void` member can't be called as the
    // `() => void` the palette maps over. See DIGIT_CHORD / KTD1.
    private onSelectTab?: (n: number) => void,
  ) {
    this.matchLeader = parseLeader(leaderKey);
  }

  /**
   * Process a key event. Returns `true` if the event was consumed by the app
   * (and must NOT reach the PTY), `false` to pass it through.
   */
  handle(e: KeyboardEvent): boolean {
    if (e.type !== "keydown") return false;

    if (this.leaderPending) {
      // A bare modifier keydown (Shift/Ctrl/Alt/Meta) fires before the key it
      // modifies — pressing Shift to type `?` or `X` arrives as its own event.
      // Don't let it consume the pending leader; keep waiting for the real key,
      // or shifted chords (`?`, `X`, `|`, `_`) would never fire.
      if (isModifierKey(e.key)) return true;
      this.leaderPending = false;
      this.dispatch(e);
      return true; // the command key never reaches the shell
    }

    if (this.matchLeader(e)) {
      this.leaderPending = true;
      return true; // swallow the leader itself
    }

    return false; // ordinary input → straight to the PTY
  }

  private dispatch(e: KeyboardEvent): void {
    // Digit chords (leader 1–9) select the Nth tab in the active workspace (U1).
    // Handled here, not via BINDINGS, because the action is parameterized. `0`,
    // shifted digits (`!@#…`), and numpad keys with NumLock off arrive as
    // something other than "1"–"9", so they fall through to the BINDINGS
    // no-match path below and become a consumed no-op like any unbound chord.
    if (this.onSelectTab && /^[1-9]$/.test(e.key)) {
      this.onSelectTab(Number(e.key));
      return;
    }
    const lower = e.key.toLowerCase();
    // Prefer a case-sensitive (`upper`) binding when the literal key is the
    // uppercase form — this is what splits `leader X` (close tab) from
    // `leader x` (close pane). `e.key !== lower` is true exactly when the
    // produced character is uppercase, whether via Shift or CapsLock, so the
    // distinction is CapsLock-robust (KTD2). Everything else matches on the
    // lowercased key, leaving `?`, `|`, `_` and CapsLocked letters unaffected.
    const binding =
      BINDINGS.find((b) => b.upper && e.key !== lower && b.keys.includes(lower)) ??
      BINDINGS.find((b) => !b.upper && b.keys.includes(lower));
    if (binding) this.actions[binding.action]();
    // An unmatched leader chord is a consumed no-op (the caller still returns
    // `true`, so it never leaks to the PTY).
  }
}
