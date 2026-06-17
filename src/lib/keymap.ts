// Keyboard model (U6): a configurable leader key gates app actions; everything
// else passes straight through to the PTY (R6). tmux-style — press the leader
// (default Ctrl-A), then a command key.

export interface KeymapActions {
  newTab: () => void;
  splitHorizontal: () => void;
  splitVertical: () => void;
  closePane: () => void;
  focusLeft: () => void;
  focusRight: () => void;
  focusUp: () => void;
  focusDown: () => void;
  cycleAttention: () => void;
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
    switch (e.key.toLowerCase()) {
      case "c":
        this.actions.newTab();
        break;
      case "|":
      case "\\":
        this.actions.splitHorizontal();
        break;
      case "-":
      case "_":
        this.actions.splitVertical();
        break;
      case "x":
        this.actions.closePane();
        break;
      case "h":
      case "arrowleft":
        this.actions.focusLeft();
        break;
      case "l":
      case "arrowright":
        this.actions.focusRight();
        break;
      case "k":
      case "arrowup":
        this.actions.focusUp();
        break;
      case "j":
      case "arrowdown":
        this.actions.focusDown();
        break;
      case "u":
        this.actions.cycleAttention();
        break;
      default:
        break; // unbound leader chord → no-op, but still consumed (no leak)
    }
  }
}
