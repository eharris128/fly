// Pure resume-command builder (U5; R4/R5/R8, KTD-C).
//
// Turns a resume record + the configured flag floor into the exact argv a
// resumed pane should run — or `null` for a bare shell. All the flag hygiene
// lives here, in one tested place, because this is the unit most likely to
// harbor a flag bug: replaying a stale `--resume`/`--continue` or a one-shot
// positional prompt would mis-resume or re-send the prompt.
//
// The auto-run this enables is the scoped KTD10 exception — gated to resume +
// a detected agent + the known `claude` shape (see KTD-E in App/Terminal).
import type { ResumeRecord } from "../ipc";

/** JS runtimes that wrap a JS Claude entrypoint (npm-global installs). */
const JS_RUNTIMES = new Set(["node", "bun", "deno"]);

/**
 * Claude flags that consume a following value. Used only to avoid mistaking a
 * flag's value for a trailing positional prompt — best-effort, since the config
 * flag floor (R8) is the real backstop, so an omission here is not load-bearing
 * (at worst a value is dropped or a prompt is kept, both recoverable). The
 * dominant default flag `--dangerously-skip-permissions` is boolean and
 * deliberately absent.
 */
const VALUE_FLAGS = new Set([
  "--model",
  "-m",
  "--fallback-model",
  "--permission-mode",
  "--permission-prompt-tool",
  "--mcp-config",
  "--append-system-prompt",
  "--settings",
  "--setting-sources",
  "--agents",
  "--session-id",
  "--output-format",
  "--input-format",
]);

/**
 * Claude flags declared `<value...>` in the CLI — they consume every following
 * non-dash token as a value, not just one (`--add-dir <directories...>`
 * verified against `claude --help`). Checked before VALUE_FLAGS so a multi-dir
 * `--add-dir /a /b` resumes with every dir instead of keeping only the first
 * and dropping the rest as bare positionals. Consumption stops at a token
 * containing whitespace: a directory argv token is effectively never
 * multi-word, but a trailing positional prompt almost always is, and re-firing
 * a prompt is the failure this module exists to prevent — the same
 * recoverable-drop tradeoff VALUE_FLAGS accepts.
 */
const VARIADIC_FLAGS = new Set(["--add-dir"]);

/** The basename of a `/`-separated path. */
function basename(s: string): string {
  const i = s.lastIndexOf("/");
  return i === -1 ? s : s.slice(i + 1);
}

/**
 * The binary-invocation prefix to preserve verbatim: `[claude]` for a native
 * binary/symlink, or `[node, …/cli.js]` for a JS-runtime wrapper. Mirrors the
 * shapes `is_claude` accepts (KTD-D), so a captured argv resumes the same way it
 * was launched.
 */
function binaryPrefix(argv: string[]): string[] {
  if (argv.length >= 2 && JS_RUNTIMES.has(basename(argv[0]))) {
    return argv.slice(0, 2);
  }
  return argv.slice(0, 1);
}

/**
 * Strip any pre-existing `--resume`/`-r`/`--continue`/`-c` (and the resume id
 * value) and every positional prompt from the flag tokens (argv after the
 * binary prefix). Replaying a resume/continue would fight the one we append;
 * replaying a prompt would re-send it. Positionals are dropped wherever they
 * sit, not just trailing: a handoff pane's argv is `claude <prompt> --add-dir
 * <dir>` — prompt BEFORE the flag, because variadic `--add-dir` would swallow a
 * trailing one (session-handoff U2/U4, KTD3) — and a resumed handoff must not
 * re-fire the pickup prompt. A VARIADIC_FLAGS flag keeps ALL consecutive
 * following non-dash values (`--add-dir /a /b` resumes with both dirs), not
 * just the first; a VALUE_FLAGS flag keeps exactly one. The cost is that an
 * unknown value-flag's value (absent from both sets) is dropped too — the
 * recoverable outcome VALUE_FLAGS already accepts.
 */
function sanitizeFlags(rest: string[]): string[] {
  const kept: string[] = [];
  for (let i = 0; i < rest.length; i++) {
    const tok = rest[i];
    // Resume/continue, separate-value form: drop the flag and its id value.
    if (tok === "--resume" || tok === "-r") {
      const next = rest[i + 1];
      if (next !== undefined && !next.startsWith("-")) i++;
      continue;
    }
    if (tok === "--continue" || tok === "-c") continue;
    // …and the `--flag=value` form.
    if (
      tok.startsWith("--resume=") ||
      tok.startsWith("-r=") ||
      tok.startsWith("--continue=") ||
      tok.startsWith("-c=")
    ) {
      continue;
    }
    if (tok.startsWith("-")) {
      kept.push(tok);
      // A variadic flag keeps every consecutive following value token (see
      // VARIADIC_FLAGS for the whitespace stop rule that spares a trailing
      // prompt from being consumed as a value).
      if (VARIADIC_FLAGS.has(tok)) {
        while (
          i + 1 < rest.length &&
          !rest[i + 1].startsWith("-") &&
          !/\s/.test(rest[i + 1])
        ) {
          kept.push(rest[i + 1]);
          i++;
        }
        continue;
      }
      // Keep a known value-flag's value so it isn't later read as the prompt.
      const next = rest[i + 1];
      if (VALUE_FLAGS.has(tok) && next !== undefined && !next.startsWith("-")) {
        kept.push(next);
        i++;
      }
      continue;
    }
    // A bare token not consumed as a value → a positional prompt: drop it
    // (re-sending it would re-prompt).
  }
  return kept;
}

/**
 * Build the argv a resumed pane should run, or `null` for a bare shell.
 *
 * - No record (not an agent) → `null`.
 * - Record with captured argv → preserve the binary prefix + replayed flags,
 *   then append `--resume <id>` (id present) or `--continue` (no id). Replaying
 *   the captured argv re-supplies `--dangerously-skip-permissions`, which Claude
 *   otherwise drops across a resume (#21974).
 * - Record without argv (renderer crash, or a pane the poll never saw) →
 *   `claude` + the resume/continue flag + the configured `defaultArgs` floor, so
 *   the permission posture is never lost (R8).
 */
export function buildResumeCommand(
  record: ResumeRecord | null | undefined,
  defaultArgs: string[],
): string[] | null {
  if (!record) return null;
  const resumeFlag = record.sessionId
    ? ["--resume", record.sessionId]
    : ["--continue"];
  const argv = record.argv;
  if (argv && argv.length > 0) {
    const prefix = binaryPrefix(argv);
    const flags = sanitizeFlags(argv.slice(prefix.length));
    return [...prefix, ...flags, ...resumeFlag];
  }
  return ["claude", ...resumeFlag, ...defaultArgs];
}

/** How a restored agent leaf re-attached: precisely (`--resume <id>`) or not. */
export type ResumeTier = "precise" | "imprecise";

/**
 * Whether a `--continue` candidate is fresh enough to be this pane's session, or
 * stale and must not be resurrected (fix-003 U3, KTD-C). The signal is the
 * candidate's **last real turn** vs the pane's own captured activity — NOT file
 * mtime, which a metadata-only `--continue` open bumps without adding a turn
 * (exactly why the 06-19 session looked "recent"). Stale when the candidate's last
 * turn predates the pane's activity by more than `marginMs` (a small clock-jitter
 * allowance). A missing candidate timestamp is treated as stale: prefer a clean
 * shell over a wrong session (R4). Pure, so the exact bug is pinned by a test.
 */
export function resumeStaleVerdict({
  candidateLastTurnMs,
  paneActivityMs,
  marginMs,
}: {
  candidateLastTurnMs: number | null;
  paneActivityMs: number;
  marginMs: number;
}): "fresh" | "stale" {
  if (candidateLastTurnMs == null) return "stale";
  return candidateLastTurnMs >= paneActivityMs - marginMs ? "fresh" : "stale";
}

/**
 * Whether a resumed leaf re-attached precisely (a captured session id →
 * `--resume <id>`) or imprecisely (no id → `--continue`, most-recent-in-folder),
 * so the degraded path is never silently presented as exact (fix-003 U3/U4,
 * KTD-D). Pure over the record.
 */
export function classifyResumeTier(
  record: ResumeRecord | null | undefined,
): ResumeTier {
  return record?.sessionId ? "precise" : "imprecise";
}

/**
 * The keep/drop decision for one restored leaf (fix-003 U3). Given the leaf's
 * record and — for an imprecise leaf — the `--continue` target's last-turn time,
 * returns its tier and whether it survives the stale-guard. A **precise** leaf
 * always keeps: the pane ran that exact session, so re-attaching it bypasses the
 * guard. An **imprecise** leaf keeps only when its `--continue` candidate is fresh
 * vs the pane's own activity (KTD-C); a stale candidate drops to a bare shell (R4).
 * Pure (the candidate timestamp is injected, the IPC fetch stays in the caller), so
 * the orchestration's core decision is tested without the app.
 */
export function resumeLeafDecision(
  record: ResumeRecord | null | undefined,
  candidateLastTurnMs: number | null,
  marginMs: number,
): { tier: ResumeTier; keep: boolean } {
  const tier = classifyResumeTier(record);
  if (tier === "precise") return { tier, keep: true };
  const verdict = resumeStaleVerdict({
    candidateLastTurnMs,
    paneActivityMs: record?.updatedAt ?? 0,
    marginMs,
  });
  return { tier, keep: verdict === "fresh" };
}

/**
 * Fold a tier map to its counts (fix-003 U4): how many resumed leaves re-attached
 * precisely (`--resume <id>`) vs imprecisely (`--continue`). Drives the offer
 * breakdown and the explicit-resume notice, so a degraded resume is surfaced and
 * never silently presented as exact (R5). Pure.
 */
export function resumeTierSummary(
  tierByLeaf: Record<string, ResumeTier>,
): { precise: number; imprecise: number } {
  let precise = 0;
  let imprecise = 0;
  for (const tier of Object.values(tierByLeaf)) {
    if (tier === "precise") precise++;
    else imprecise++;
  }
  return { precise, imprecise };
}

/**
 * Decide which restored leaves resume, with what tier, and how many were dropped
 * to a bare shell by the stale-guard (fix-003 U3). Pure: each imprecise leaf's
 * `--continue` candidate freshness (`candidateLastTurnByLeaf`, pre-fetched by the
 * caller) is injected, so the keep/drop/classify composition — the heart of the
 * fix, and the plan's U3 "Logic" scenario — is tested without IPC. Takes the
 * already-built `commands` (from {@link resumeCommandsForLeaves}); a precise leaf
 * keeps and stays `--resume <id>`; an imprecise leaf keeps only when fresh, else
 * it is removed (→ bare shell, R4) and counted in `staleDropped` so the caller can
 * disclose it (R5/AE3).
 */
export function planResumeLeaves(
  commands: Record<string, string[]>,
  records: Record<string, ResumeRecord | undefined>,
  candidateLastTurnByLeaf: Record<string, number | null>,
  marginMs: number,
): {
  commands: Record<string, string[]>;
  tierByLeaf: Record<string, ResumeTier>;
  staleDropped: number;
} {
  const kept: Record<string, string[]> = {};
  const tierByLeaf: Record<string, ResumeTier> = {};
  let staleDropped = 0;
  for (const key of Object.keys(commands)) {
    const { tier, keep } = resumeLeafDecision(
      records[key],
      candidateLastTurnByLeaf[key] ?? null,
      marginMs,
    );
    if (!keep) {
      staleDropped++; // stale → bare shell, but disclosed (R5/AE3)
      continue;
    }
    kept[key] = commands[key];
    tierByLeaf[key] = tier;
  }
  return { commands: kept, tierByLeaf, staleDropped };
}

/**
 * Whether a restored leaf's resume is ambiguity-risky (fix-session-pane-
 * attribution U9, R13/AE5): its stored id is no better than the poll's
 * cwd-level guess (source `poll` — a pre-fix record loads the same) AND its
 * cwd holds more than one qualifying transcript at resume time, so
 * `--resume`/`--continue` could re-attach a sibling's session. A hook- or
 * pick-sourced id is pane-precise and never flagged. Keyed on transcript
 * count, not live freshness: crash-resume runs at startup when nothing is
 * fresh, so a freshness signal would be structurally zero and never fire.
 * Pure — the count is injected (the IPC probe stays in the caller).
 */
export function isAmbiguousResumeLeaf(
  record: ResumeRecord | null | undefined,
  qualifyingTranscriptCount: number,
): boolean {
  const source = record?.sessionSource ?? "poll";
  if (source === "hook" || source === "pick") return false;
  return qualifyingTranscriptCount > 1;
}

/**
 * The resume-offer dialog's tier breakdown (fix-003 U4, R5; fix-attribution
 * U9): `null` when every resumable leaf is exact and unambiguous (nothing to
 * disclose), else a terse "M exact · K most-recent-in-folder · J stale,
 * started fresh · N in multi-session folders" line. `ambiguous` counts the
 * higher-risk leaves — several qualifying transcripts share their cwd, so the
 * resume may re-attach a sibling (R13/AE5). Pure.
 */
export function resumeOfferBreakdown(
  tiers: { precise: number; imprecise: number },
  staleDropped: number,
  ambiguous = 0,
): string | null {
  if (tiers.imprecise === 0 && staleDropped === 0 && ambiguous === 0) return null;
  const parts: string[] = [];
  if (tiers.precise > 0) parts.push(`${tiers.precise} exact`);
  if (tiers.imprecise > 0) parts.push(`${tiers.imprecise} most-recent-in-folder`);
  if (staleDropped > 0) parts.push(`${staleDropped} stale, started fresh`);
  if (ambiguous > 0) {
    parts.push(
      `${ambiguous} in multi-session folders — may re-attach a sibling`,
    );
  }
  return parts.join(" · ");
}

/**
 * The transient post-resume disclosure for the explicit `fly resume` path, which
 * shows no offer dialog (fix-003 U4, R5/AE3; fix-attribution U9): names how many
 * panes re-attached imprecisely (`--continue`), how many were dropped to a fresh
 * shell because their only candidate session was stale, and how many resumed
 * from a multi-session folder where a sibling could have been re-attached
 * (R13/AE5). `null` when everything resumed exactly and unambiguously. Pure, so
 * the wording is unit-tested.
 */
export function resumeNoticeText(
  summary: { precise: number; imprecise: number },
  staleDropped: number,
  ambiguous = 0,
): string | null {
  const parts: string[] = [];
  if (summary.imprecise > 0) {
    const s = summary.imprecise === 1 ? "" : "s";
    parts.push(`${summary.imprecise} agent${s} resumed by most-recent session in folder`);
  }
  if (staleDropped > 0) {
    const s = staleDropped === 1 ? "" : "s";
    parts.push(`${staleDropped} agent${s} started fresh — no recent session to resume`);
  }
  if (ambiguous > 0) {
    const s = ambiguous === 1 ? "" : "s";
    parts.push(
      `${ambiguous} agent${s} resumed from a folder with several sessions — verify the right one re-attached`,
    );
  }
  return parts.length ? parts.join("; ") + "." : null;
}

/**
 * Whether the poll should capture a leaf's resolved session id (fix-003 U2,
 * KTD-B). Unlike argv — fixed for a pane's life, captured once — a session id
 * rotates within a pane's life (`/clear`, a new conversation), so capture is
 * change-tracked: write through only when the resolved id is present **and**
 * differs from the last one seen for that leaf. A `null` resolution (no active
 * transcript / not an agent) is a skip, so a transient miss never clears a
 * previously-captured id. Pure, so the poll's guard is tested without the app.
 */
export function shouldCaptureSession(
  lastSeen: string | null,
  resolved: string | null,
): boolean {
  if (resolved == null) return false;
  return resolved !== lastSeen;
}

/**
 * Build the resume command for each restored leaf that has a record producing
 * one (U8). Leaves with no record (or whose record yields no command) are
 * omitted, so the caller treats a missing entry as "bare shell". Pure, so the
 * restore wiring is testable without the app.
 */
export function resumeCommandsForLeaves(
  leafKeys: Iterable<string>,
  records: Record<string, ResumeRecord | undefined>,
  defaultArgs: string[],
): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  for (const key of leafKeys) {
    const cmd = buildResumeCommand(records[key], defaultArgs);
    if (cmd) out[key] = cmd;
  }
  return out;
}
