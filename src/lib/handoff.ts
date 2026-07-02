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
