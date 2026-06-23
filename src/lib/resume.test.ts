import { describe, it, expect } from "vitest";
import {
  buildResumeCommand,
  resumeCommandsForLeaves,
  shouldCaptureSession,
} from "./resume";
import type { ResumeRecord } from "../ipc";

// The configured flag floor (R8): replayed when no argv was captured.
const DEFAULT = ["--dangerously-skip-permissions"];

function rec(over: Partial<ResumeRecord>): ResumeRecord {
  return {
    sessionId: null,
    sessionCwd: null,
    argv: null,
    isAgent: false,
    updatedAt: 0,
    ...over,
  };
}

describe("buildResumeCommand (U5)", () => {
  it("returns null for no record → bare shell", () => {
    expect(buildResumeCommand(null, DEFAULT)).toBeNull();
    expect(buildResumeCommand(undefined, DEFAULT)).toBeNull();
  });

  it("appends --resume <id> after replayed flags, preserving the skip flag", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--dangerously-skip-permissions"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--dangerously-skip-permissions", "--resume", "x"]);
  });

  it("preserves a --model value flag", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--model", "opus", "--dangerously-skip-permissions"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual([
      "claude",
      "--model",
      "opus",
      "--dangerously-skip-permissions",
      "--resume",
      "x",
    ]);
  });

  it("falls back to the flag floor with --resume <id> when no argv was captured", () => {
    const out = buildResumeCommand(rec({ sessionId: "abc", isAgent: true }), DEFAULT);
    expect(out).toEqual(["claude", "--resume", "abc", "--dangerously-skip-permissions"]);
  });

  it("uses --continue + flag floor for an agent with neither id nor argv", () => {
    const out = buildResumeCommand(rec({ isAgent: true }), DEFAULT);
    expect(out).toEqual(["claude", "--continue", "--dangerously-skip-permissions"]);
  });

  it("strips an existing --continue and appends --resume <id>", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--continue"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--resume", "x"]);
  });

  it("strips a pre-existing --resume and its id value", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "-r", "stale-id", "--model", "opus"], sessionId: "new" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--model", "opus", "--resume", "new"]);
  });

  it("strips the --resume=<id> form", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--resume=stale"], sessionId: "new" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--resume", "new"]);
  });

  it("strips a trailing positional prompt after a boolean flag (no re-send)", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--dangerously-skip-permissions", "write a poem"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--dangerously-skip-permissions", "--resume", "x"]);
  });

  it("strips a trailing prompt but keeps a value-flag's value", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--model", "opus", "explain this code"], sessionId: "x" }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--model", "opus", "--resume", "x"]);
  });

  it("preserves a node-wrapper argv[0..2] and appends --resume <id>", () => {
    const out = buildResumeCommand(
      rec({
        argv: ["node", "/home/u/.npm/lib/node_modules/claude/cli.js", "--dangerously-skip-permissions"],
        sessionId: "x",
      }),
      DEFAULT,
    );
    expect(out).toEqual([
      "node",
      "/home/u/.npm/lib/node_modules/claude/cli.js",
      "--dangerously-skip-permissions",
      "--resume",
      "x",
    ]);
  });

  it("uses --continue with replayed flags when argv is present but no id", () => {
    const out = buildResumeCommand(
      rec({ argv: ["claude", "--model", "opus"], isAgent: true }),
      DEFAULT,
    );
    expect(out).toEqual(["claude", "--model", "opus", "--continue"]);
  });

  it("treats an empty argv as no argv (uses the floor)", () => {
    const out = buildResumeCommand(rec({ argv: [], sessionId: "x" }), DEFAULT);
    expect(out).toEqual(["claude", "--resume", "x", "--dangerously-skip-permissions"]);
  });
});

describe("shouldCaptureSession (fix-003 U2, KTD-B)", () => {
  it("captures the first time an id is seen (lastSeen null)", () => {
    expect(shouldCaptureSession(null, "sess-A")).toBe(true);
  });

  it("skips when the id is unchanged", () => {
    expect(shouldCaptureSession("sess-A", "sess-A")).toBe(false);
  });

  it("captures when the id changes (/clear, new conversation)", () => {
    expect(shouldCaptureSession("sess-A", "sess-B")).toBe(true);
  });

  it("skips a null resolution, preserving the last captured id", () => {
    // A transient miss (no active transcript) must not clear what we have.
    expect(shouldCaptureSession("sess-A", null)).toBe(false);
    expect(shouldCaptureSession(null, null)).toBe(false);
  });
});

describe("resumeCommandsForLeaves (U8)", () => {
  it("maps each restored leaf to its command, omitting bare-shell leaves", () => {
    const records = {
      "leaf-1": rec({ argv: ["claude", "--model", "opus"], sessionId: "a" }),
      "leaf-2": rec({ sessionId: "b", isAgent: true }), // no argv → floor
      // leaf-3 has no record → bare shell, omitted from the result
    };
    const out = resumeCommandsForLeaves(["leaf-1", "leaf-2", "leaf-3"], records, DEFAULT);
    expect(out).toEqual({
      "leaf-1": ["claude", "--model", "opus", "--resume", "a"],
      "leaf-2": ["claude", "--resume", "b", "--dangerously-skip-permissions"],
    });
    expect(out["leaf-3"]).toBeUndefined();
  });

  it("ignores orphan records whose leaf isn't in the layout", () => {
    const records = {
      "leaf-1": rec({ sessionId: "a", isAgent: true }),
      "gone-leaf": rec({ sessionId: "z", isAgent: true }),
    };
    const out = resumeCommandsForLeaves(["leaf-1"], records, DEFAULT);
    expect(Object.keys(out)).toEqual(["leaf-1"]);
  });
});
