// Session handoff (U2, docs/plans/2026-07-02-001-feat-session-handoff-plan.md):
// the pure command + prompt builder behind the two handoff chords. A pure data
// module like `automations.ts` / `keymap.ts` — no DOM, no Svelte, no ipc calls
// — so it is unit-tested without an app. App.svelte resolves the target over
// IPC (U1), then seeds `handoffCommandByLeaf[newLeaf]` from
// `buildHandoffCommand` so the Terminal reads the argv once at mount.
//
// The argv is exec'd directly (no shell), and the prompt rides as the trailing
// positional argument (R7) — the resume capture's `sanitizeFlags` strips
// positionals, so a restart never re-fires the pickup prompt (plan KTD3).

import type { HandoffTarget } from "../ipc";

/** Quick sends the stock prompt as trailing argv (R7); guided spawns bare
 *  `claude` and U3's injection controller pre-types the prompt unsent (R9). */
export type HandoffMode = "quick" | "guided";

/**
 * App-side registry of guided handoff panes, exported as U3's contract: leaf
 * key → the resolved target whose prompt the injection controller pre-types
 * into that pane's composer. U2 only populates it (in App.svelte, at the same
 * synchronous seed as `handoffCommandByLeaf`); quick panes never appear here.
 */
export type GuidedHandoffByLeaf = Record<string, HandoffTarget>;

/**
 * Strip control characters (C0 incl. newlines, DEL, C1) from a transcript path
 * before it is embedded anywhere. The path is backend-derived so a control
 * character means corruption or forgery — same write-time-sanitization posture
 * as the alerts log (automations R16). Stripping (not rejecting) keeps the
 * chord total: a mangled path at worst fails the agent's read, visibly.
 */
function sanitizeTranscriptPath(path: string): string {
  return path.replace(/[\u0000-\u001f\u007f-\u009f]/g, "");
}

/**
 * The stock pickup prompt (R8; initial wording, tuned against real sessions in
 * U4). Names the exact transcript path, directs reading its RECENT portion —
 * the tail, never the whole 200k-token file — determining the outstanding
 * work, and continuing it. Exported for U3, whose injection payload is this
 * same prompt pre-typed unsent.
 */
export function handoffPrompt(transcriptPath: string): string {
  const path = sanitizeTranscriptPath(transcriptPath);
  return (
    `You are taking over from a previous Claude Code session in this directory. ` +
    `Its transcript is at ${path}. Read the RECENT portion of that file (tail it — ` +
    `do not read the whole file), determine what was in progress and what work ` +
    `remains outstanding, then continue that work.`
  );
}

/**
 * Build the handoff pane's argv (U2). Both modes launch `claude` in the user's
 * default permission mode — never `--dangerously-skip-permissions` — with
 * `--add-dir <transcript's project dir>` so the first transcript read (outside
 * the pane's cwd) needs no approval (R10). Quick appends the stock prompt as
 * the trailing positional so the fresh instance starts working with no further
 * input (R7); guided omits it (U3 pre-types instead, R9). The project dir is
 * the dirname of the sanitized transcript path — not a separate target field.
 */
export function buildHandoffCommand(
  target: HandoffTarget,
  mode: HandoffMode,
): string[] {
  const path = sanitizeTranscriptPath(target.transcriptPath);
  const projectDir = path.replace(/\/[^/]*$/, "") || "/";
  const argv = ["claude", "--add-dir", projectDir];
  if (mode === "quick") argv.push(handoffPrompt(path));
  return argv;
}

// ---- U3: guided injection controller ----------------------------------------
// (docs/plans/2026-07-02-001-feat-session-handoff-plan.md, U3/R9.) Guided
// handoff spawns bare `claude`; this pure state machine decides when — and
// whether — to pre-type the stock prompt into its composer, unsent. Repo
// convention (src-tauri/src/state/attention.rs, layout.ts): time and inputs
// are arguments, never sampled here, so every transition is unit-testable.
// The wiring (Terminal.svelte) feeds real timestamps plus a ticker and
// performs the single `inject` effect via the `ptyWrite` IPC.

/**
 * Readiness-heuristic timings. Named exports so U4 can tune them against real
 * Claude Code startup (including a slow first paint) without touching the
 * reducer — the tests derive their timelines from these, so tuning cannot
 * break them.
 */
/** Quiet gap after the last output chunk before the composer counts as ready
 *  (the startup paint has settled). Initial candidate per the plan; U4 tunes. */
export const INJECT_QUIET_GAP_MS = 400;
/** Overall cap on waiting for readiness, anchored at spawn. At/past it the
 *  controller resolves to `skipped` — even when a quiet gap is observable on
 *  the very same tick — so a stalled or odd startup never becomes a surprise
 *  late injection; the pane stays a usable bare `claude`. */
export const INJECT_TIMEOUT_MS = 10_000;

/**
 * Everything the controller can observe, timestamped by the caller:
 * PTY output chunks, user-originated input, the pane dying, and a periodic
 * ticker (the only event that can *decide* — readiness and timeout are both
 * "enough time passed with nothing happening", which only a tick can see).
 * The plan's `spawned` event is the initial state: [`injectionSpawned`].
 */
export type InjectionEvent =
  | { kind: "output"; t: number }
  | { kind: "userInput" }
  | { kind: "paneExit" }
  | { kind: "tick"; t: number };

/**
 * `spawned` is the sole live phase; the rest are terminal. The plan's `ready`
 * has no duration — it is the instant a tick observes the quiet gap — so the
 * reducer folds spawned → ready → injected into one transition that emits the
 * inject effect. Terminal escapes (R9's safety half): `skipped` when the user
 * typed first (their intent wins — never interleave with their typing) or the
 * timeout lapsed; `cancelled` when the pane exited.
 */
export type InjectionPhase = "spawned" | "injected" | "skipped" | "cancelled";

export interface InjectionState {
  phase: InjectionPhase;
  /** Wall-clock ms at spawn — the timeout anchor. */
  spawnedAt: number;
  /** Wall-clock ms of the last output chunk; null until the first one. */
  lastOutputAt: number | null;
}

/** One step's outcome: the next state, plus whether to perform the one and
 *  only `inject` effect (write the payload to the pane's PTY) right now. */
export interface InjectionResult {
  state: InjectionState;
  inject: boolean;
}

/** The `spawned` event — the controller's initial state, minted when the
 *  guided pane spawns. `t` anchors the overall timeout. */
export function injectionSpawned(t: number): InjectionState {
  return { phase: "spawned", spawnedAt: t, lastOutputAt: null };
}

/** True once the controller can never inject again — the wiring's cue to stop
 *  the ticker and release the pane's `guidedHandoffByLeaf` entry. */
export function injectionDone(state: InjectionState): boolean {
  return state.phase !== "spawned";
}

/**
 * The reducer. Emits `inject: true` at most once across any event sequence:
 * every path out of `spawned` lands in a terminal phase, and terminal phases
 * absorb all events unchanged (idempotent by construction).
 */
export function injectionStep(
  state: InjectionState,
  ev: InjectionEvent,
): InjectionResult {
  if (state.phase !== "spawned") return { state, inject: false };
  switch (ev.kind) {
    case "output":
      // Not a decision point — new output while deciding means the paint has
      // NOT settled, so injecting here could interleave. Just re-arm the gap.
      return { state: { ...state, lastOutputAt: ev.t }, inject: false };
    case "userInput":
      return { state: { ...state, phase: "skipped" }, inject: false };
    case "paneExit":
      return { state: { ...state, phase: "cancelled" }, inject: false };
    case "tick": {
      // Timeout first: it caps readiness (see INJECT_TIMEOUT_MS).
      if (ev.t - state.spawnedAt >= INJECT_TIMEOUT_MS)
        return { state: { ...state, phase: "skipped" }, inject: false };
      if (
        state.lastOutputAt !== null &&
        ev.t - state.lastOutputAt >= INJECT_QUIET_GAP_MS
      )
        return { state: { ...state, phase: "injected" }, inject: true };
      return { state, inject: false };
    }
  }
}

/**
 * Build the injection payload from the stock prompt (R9): newlines normalized
 * to LF, every other control character stripped — ESC among them, so the inner
 * text can never forge the paste-end marker and break out of the wrap — then
 * wrapped in bracketed-paste markers so embedded newlines land in the composer
 * instead of submitting. NO trailing carriage return: nothing is sent until
 * the user presses Enter, and their typed text appends after the prefix.
 */
export function injectionPayload(text: string): string {
  const inner = text
    .replace(/\r\n?/g, "\n")
    .replace(/[\x00-\x09\x0b-\x1f\x7f-\x9f]/g, "");
  return `\x1b[200~${inner}\x1b[201~`;
}
