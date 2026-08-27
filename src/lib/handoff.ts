// Session handoff (U2, docs/plans/2026-07-02-001-feat-session-handoff-plan.md):
// the pure command + prompt builder behind the two handoff chords. A pure data
// module like `automations.ts` / `keymap.ts` — no DOM, no Svelte, no ipc calls
// — so it is unit-tested without an app. App.svelte resolves the target over
// IPC (U1), then seeds `handoffCommandByLeaf[newLeaf]` from
// `buildHandoffCommand` so the Terminal reads the argv once at mount.
//
// The argv is exec'd directly (no shell), and the prompt rides as a positional
// argument BEFORE `--add-dir` (R7; the variadic flag would swallow a trailing
// one) — the resume capture's `sanitizeFlags` strips positionals wherever they
// sit, so a restart never re-fires the pickup prompt (plan KTD3).

import type { HandoffTarget } from "../ipc";

/** Quick launches bypass-permissions with the stock prompt as trailing argv
 *  (R7); guided spawns bare `claude` in the default permission mode and U3's
 *  injection controller pre-types the prompt unsent (R9). */
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
 * Exported for monitor pickup (monitor-handoff U7), which embeds the same
 * kind of untrusted stored paths in its prompt and its fallback display.
 */
export function sanitizeTranscriptPath(path: string): string {
  return stripControlChars(path);
}

/** C0 (incl. newlines), DEL, and C1 - the control-char class every untrusted
 *  frontend display/embed string is stripped of (the frontend mirror of the
 *  backend's `notify::sanitize_*` posture). */
const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/g;

/**
 * Replace control characters in an untrusted string - removed by default, or
 * swapped for `replacement` (e.g. `" "` to keep word boundaries when
 * flattening a multi-line note). The one shared control-char sanitizer
 * (monitor-handoff U7 reuses it for verdict notes and bundle text).
 */
export function stripControlChars(text: string, replacement = ""): string {
  return text.replace(CONTROL_CHARS, replacement);
}

/** Dirname of a slash path (`"/"` for a bare filename) - the `--add-dir`
 *  project-dir derivation shared by handoff and monitor pickup. */
function dirnameOf(path: string): string {
  return path.replace(/\/[^/]*$/, "") || "/";
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
 * Build the handoff pane's argv (U2). Both modes launch `claude` with
 * `--add-dir <transcript's project dir>` so the first transcript read (outside
 * the pane's cwd) needs no approval (R10; verified in U4: without the flag the
 * read is permission-denied, with it it succeeds unprompted). The project dir
 * is the dirname of the sanitized transcript path — not a separate target field.
 *
 * Quick launches in **bypass-permissions mode** (`--dangerously-skip-permissions`)
 * and carries the stock prompt as the positional so the fresh instance starts
 * working with no further input (R7): a quick resume runs the prompt
 * immediately and unattended, so a mid-work permission prompt would just stall
 * it. Guided omits both — it stays in the user's default permission mode
 * because the user is in the loop (U3 pre-types the prompt unsent for them to
 * review and send, R9). The prompt MUST precede `--add-dir`: the flag is
 * variadic (`<directories...>`), so a trailing positional would be swallowed as
 * another directory (U4's runtime check — `claude -p --add-dir <dir> "<prompt>"`
 * errors with "input must be provided"); the boolean skip-permissions flag can
 * sit anywhere, so it leads.
 */
export function buildHandoffCommand(
  target: HandoffTarget,
  mode: HandoffMode,
): string[] {
  const path = sanitizeTranscriptPath(target.transcriptPath);
  const projectDir = dirnameOf(path);
  const argv = ["claude"];
  if (mode === "quick") argv.push("--dangerously-skip-permissions", handoffPrompt(path));
  argv.push("--add-dir", projectDir);
  return argv;
}

// ---- monitor pickup (monitor-handoff U7, R16) -------------------------------
// A failed monitor's one-action pickup mirrors the handoff prompt/argv shape:
// same sanitization, same "prompt positional BEFORE the variadic --add-dir"
// ordering lesson, same tail-not-whole-file transcript guidance. Unlike quick
// handoff it stays in the user's DEFAULT permission mode — the user is present
// at pickup (they clicked the button), so no --dangerously-skip-permissions.

/**
 * The stock pickup prompt for a failed monitor's recovery session (R16):
 * points the fresh agent at the failure bundle (verdict + evidence + pointers)
 * and the parent transcript's RECENT portion — the tail, never the whole
 * file — then directs diagnosing and continuing. Both paths are stored
 * strings, control-char-sanitized before embedding. `bundlePath` may be null
 * (a failed bundle write, U3's fail-tolerance): the prompt then leans on the
 * transcript alone.
 */
export function monitorPickupPrompt(
  bundlePath: string | null,
  transcriptPath: string,
): string {
  const transcript = sanitizeTranscriptPath(transcriptPath);
  const bundle =
    bundlePath != null ? sanitizeTranscriptPath(bundlePath) : null;
  const bundleLead = bundle
    ? `Its failure bundle is at ${bundle} — read that file first: it carries the ` +
      `verdict, the failure evidence, and pointers to the original session. `
    : `Its failure bundle could not be written, so start from the transcript. `;
  return (
    `You are picking up a failed experiment that a monitor automation was ` +
    `watching. ${bundleLead}` +
    `The original session's transcript is at ${transcript}. Read the RECENT ` +
    `portion of that file (tail it — do not read the whole file) to see what ` +
    `the experiment was doing, then diagnose the failure and continue the work.`
  );
}

/**
 * Build the pickup pane's argv (monitor-handoff U7, R16). The prompt is the
 * positional and MUST precede `--add-dir` (the flag is variadic — a trailing
 * positional would be swallowed as another directory, the buildHandoffCommand
 * lesson). `--add-dir` grants the transcript's project dir and — when a
 * bundle exists outside it — the bundle's dir, so the first reads need no
 * approval. Default permission mode: the user is present at pickup, so no
 * bypass flag, ever.
 */
export function buildMonitorPickupCommand(
  transcriptPath: string,
  bundlePath: string | null,
): string[] {
  const transcript = sanitizeTranscriptPath(transcriptPath);
  const transcriptDir = dirnameOf(transcript);
  const dirs = [transcriptDir];
  if (bundlePath != null) {
    const bundleDir = dirnameOf(sanitizeTranscriptPath(bundlePath));
    if (bundleDir !== transcriptDir) dirs.push(bundleDir);
  }
  return [
    "claude",
    monitorPickupPrompt(bundlePath, transcript),
    "--add-dir",
    ...dirs,
  ];
}

// ---- U3: guided injection controller ----------------------------------------
// (docs/plans/2026-07-02-001-feat-session-handoff-plan.md, U3/R9.) Guided
// handoff spawns bare `claude`; this pure state machine decides when — and
// whether — to pre-type the stock prompt into its composer, unsent. Repo
// convention (core/src/state/attention.rs, layout.ts): time and inputs
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

/**
 * Classify an xterm `onData` chunk as user-originated input (U3/R9: the user's
 * intent wins; never interleave). The rule is two-sided:
 *
 * - An ESC-free chunk is user input (typed text, Enter, Ctrl-C): xterm answers
 *   terminal queries — cursor position (CPR), device attributes (DA) — through
 *   the same `onData` event, and every such auto-reply starts with ESC, so
 *   chunks containing ESC are excluded by default.
 * - EXCEPT a user paste: xterm delivers pastes only via `onData` (never
 *   `onKey`), wrapped in bracketed-paste markers `\x1b[200~…\x1b[201~` — so the
 *   ESC exclusion alone would let a pre-injection paste be ignored and the
 *   stock prompt still inject after it (review finding #1). Auto-replies never
 *   contain the paste-open marker, so its presence marks the chunk user input.
 *
 * ESC-prefixed *keyboard* keys (arrows, Esc itself) are out of scope here —
 * the wiring covers those via `onKey`.
 */
export function isUserInputChunk(data: string): boolean {
  return !data.includes("\x1b") || data.includes("\x1b[200~");
}
