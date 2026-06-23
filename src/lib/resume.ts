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
  "--add-dir",
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
 * value) and a single trailing positional prompt from the flag tokens (argv
 * after the binary prefix). Replaying a resume/continue would fight the one we
 * append; replaying a prompt would re-send it.
 */
function sanitizeFlags(rest: string[]): string[] {
  const kept: { tok: string; positional: boolean }[] = [];
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
      kept.push({ tok, positional: false });
      // Keep a known value-flag's value so it isn't later read as the prompt.
      const next = rest[i + 1];
      if (VALUE_FLAGS.has(tok) && next !== undefined && !next.startsWith("-")) {
        kept.push({ tok: next, positional: false });
        i++;
      }
      continue;
    }
    // A bare token not consumed as a value → a positional (candidate prompt).
    kept.push({ tok, positional: true });
  }
  // Drop a single trailing positional prompt (re-sending it would re-prompt).
  if (kept.length > 0 && kept[kept.length - 1].positional) {
    kept.pop();
  }
  return kept.map((k) => k.tok);
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
