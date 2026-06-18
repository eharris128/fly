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
];

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
