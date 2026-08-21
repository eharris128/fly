// Tests for the transport seam's Electron-bridge path — above all the
// `plainArgs` invariant: EVERY `bridge.invoke` must JSON-round-trip its args
// before the IPC hop, because Electron structured-clones its arguments and a
// Svelte 5 `$state` proxy fails that clone. The resume path shipped broken
// (commit d483f5c) because `spawnPaneWithSink` skipped the round-trip; these
// tests pin both existing call sites behaviorally AND lint the module source
// so a future `bridge.invoke` call can't reintroduce the bug.
//
// The bridge is captured from `window.fly` at module load, so each test
// installs the mock bridge on a fresh `window` and dynamic-imports a fresh
// module instance.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import transportSource from "./transport.ts?raw";

interface MockBridge {
  invoke: ReturnType<typeof vi.fn>;
  onEvent: ReturnType<typeof vi.fn>;
  onPaneOutput: ReturnType<typeof vi.fn>;
  paneInput: ReturnType<typeof vi.fn>;
}

function makeBridge(): MockBridge {
  return {
    invoke: vi.fn(async () => undefined),
    onEvent: vi.fn(() => () => {}),
    onPaneOutput: vi.fn(() => () => {}),
    paneInput: vi.fn(),
  };
}

// A stand-in for a Svelte 5 `$state` proxy: structured-clone-hostile (traps),
// but JSON-serializable by reading through the traps — exactly the shape that
// broke the resume path.
function proxyArgs<T extends object>(target: T): T {
  return new Proxy(target, {
    get(t, prop, receiver) {
      return Reflect.get(t, prop, receiver);
    },
  });
}

let bridge: MockBridge;

beforeEach(() => {
  vi.resetModules();
  bridge = makeBridge();
  (globalThis as Record<string, unknown>).window = { fly: bridge };
});

afterEach(() => {
  delete (globalThis as Record<string, unknown>).window;
});

async function loadTransport() {
  return await import("./transport");
}

describe("invoke (bridge path)", () => {
  it("passes a JSON-plain deep clone, never the caller's object", async () => {
    const t = await loadTransport();
    const args = proxyArgs({ leaf: "l1", argv: ["claude", "--resume", "abc"] });
    await t.invoke("save_thing", args);
    expect(bridge.invoke).toHaveBeenCalledTimes(1);
    const [cmd, sent] = bridge.invoke.mock.calls[0];
    expect(cmd).toBe("save_thing");
    expect(sent).not.toBe(args); // a clone, not the proxy itself
    expect(sent).toEqual({ leaf: "l1", argv: ["claude", "--resume", "abc"] });
    // The clone is plain data: JSON round-trips to itself.
    expect(JSON.parse(JSON.stringify(sent))).toEqual(sent);
  });

  it("preserves an undefined args as undefined (no {} fabrication)", async () => {
    const t = await loadTransport();
    await t.invoke("no_args_cmd");
    expect(bridge.invoke).toHaveBeenCalledWith("no_args_cmd", undefined);
  });
});

describe("spawnPaneWithSink (bridge path — the d483f5c regression)", () => {
  it("JSON round-trips the spawn args before the bridge hop", async () => {
    bridge.invoke = vi.fn(async () => 7);
    const t = await loadTransport();
    const sink = t.makeOutputSink(() => {});
    const args = proxyArgs({ cwd: "/home/u/p", argv: ["claude", "--resume"] });
    const paneId = await t.spawnPaneWithSink(sink, args);
    expect(paneId).toBe(7);
    const [cmd, sent] = bridge.invoke.mock.calls[0];
    expect(cmd).toBe("spawn_pane");
    expect(sent).not.toBe(args);
    expect(sent).toEqual({ cwd: "/home/u/p", argv: ["claude", "--resume"] });
  });

  it("flushes pre-subscription frames in order, then streams live", async () => {
    let emit: (paneId: number, bytes: Uint8Array) => void = () => {};
    bridge.onPaneOutput = vi.fn((cb) => {
      emit = cb;
      return () => {};
    });
    let resolveSpawn: (id: number) => void = () => {};
    bridge.invoke = vi.fn(
      () => new Promise<unknown>((res) => (resolveSpawn = res as never)),
    );
    const t = await loadTransport();
    const seen: string[] = [];
    const sink = t.makeOutputSink((b) => seen.push(new TextDecoder().decode(b)));
    const spawned = t.spawnPaneWithSink(sink, { argv: [] });
    // Frames race in before the renderer learns the pane id.
    emit(3, new TextEncoder().encode("early-1"));
    emit(3, new TextEncoder().encode("early-2"));
    resolveSpawn(3);
    await spawned;
    emit(3, new TextEncoder().encode("live"));
    expect(seen).toEqual(["early-1", "early-2", "live"]);
  });

  it("releasePaneSink drops the sink and any buffered frames", async () => {
    let emit: (paneId: number, bytes: Uint8Array) => void = () => {};
    bridge.onPaneOutput = vi.fn((cb) => {
      emit = cb;
      return () => {};
    });
    bridge.invoke = vi.fn(async () => 4);
    const t = await loadTransport();
    const seen: Uint8Array[] = [];
    const sink = t.makeOutputSink((b) => seen.push(b));
    await t.spawnPaneWithSink(sink, {});
    t.releasePaneSink(4);
    emit(4, new Uint8Array([1]));
    expect(seen).toEqual([]);
  });
});

describe("listen (bridge path)", () => {
  it("fans one bridge subscription out per event and honors unlisten", async () => {
    let emit: (event: string, payload: unknown) => void = () => {};
    bridge.onEvent = vi.fn((cb) => {
      emit = cb;
      return () => {};
    });
    const t = await loadTransport();
    const a: unknown[] = [];
    const b: unknown[] = [];
    const offA = await t.listen("pane://attention", (ev) => a.push(ev.payload));
    await t.listen("pane://exit", (ev) => b.push(ev.payload));
    emit("pane://attention", { paneId: 1 });
    emit("pane://exit", { paneId: 2 });
    expect(a).toEqual([{ paneId: 1 }]);
    expect(b).toEqual([{ paneId: 2 }]);
    offA();
    emit("pane://attention", { paneId: 9 });
    expect(a).toEqual([{ paneId: 1 }]);
    expect(bridge.onEvent).toHaveBeenCalledTimes(1); // one wire, fanned out
  });
});

describe("the plainArgs invariant (source lint)", () => {
  it("every bridge.invoke call site routes its args through plainArgs", () => {
    const sites = transportSource.split("bridge.invoke(").slice(1);
    expect(sites.length).toBeGreaterThanOrEqual(2); // invoke + spawnPaneWithSink
    for (const site of sites) {
      const head = site.slice(0, 80);
      const singleArg = /^[^,)]*\)/.test(head); // no args object at all — safe
      expect(
        singleArg || head.includes("plainArgs("),
        `bridge.invoke call without plainArgs(): "${head.trim()}..." — ` +
          "Electron structured-clones IPC args; a $state proxy throws. " +
          "Wrap the args in plainArgs() (see the d483f5c resume regression).",
      ).toBe(true);
    }
  });
});
