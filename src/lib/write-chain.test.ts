import { describe, expect, it } from "vitest";
import { makeWriteChain } from "./write-chain";

/** A promise with its resolvers exposed, for driving settlement order. */
function deferred<T = void>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const tick = () => new Promise<void>((r) => setTimeout(r, 0));

describe("makeWriteChain (poll-batching KTD5/R5)", () => {
  it("starts a key's second task only after the first settles", async () => {
    // The exact hazard: task 1 is slow, task 2 would complete first if allowed
    // to run concurrently (async backend workers). The chain must not even
    // START task 2 until task 1 settles.
    const chain = makeWriteChain<number>();
    const first = deferred();
    const started: string[] = [];
    void chain.run(1, () => {
      started.push("a");
      return first.promise;
    });
    void chain.run(1, () => {
      started.push("b");
      return Promise.resolve();
    });
    await tick();
    expect(started).toEqual(["a"]); // b not started while a is in flight
    first.resolve();
    await tick();
    expect(started).toEqual(["a", "b"]);
  });

  it("a failed task rejects its own caller but never wedges the chain", async () => {
    const chain = makeWriteChain<number>();
    const p1 = chain.run(1, () => Promise.reject(new Error("pty gone")));
    await expect(p1).rejects.toThrow("pty gone");
    const p2 = chain.run(1, () => Promise.resolve("ok"));
    await expect(p2).resolves.toBe("ok");
  });

  it("different keys run independently", async () => {
    const chain = makeWriteChain<number>();
    const slow = deferred();
    const started: string[] = [];
    void chain.run(1, () => {
      started.push("pane1");
      return slow.promise;
    });
    void chain.run(2, () => {
      started.push("pane2");
      return Promise.resolve();
    });
    await tick();
    // Pane 2 must not queue behind pane 1's slow write.
    expect(started).toEqual(["pane1", "pane2"]);
    slow.resolve();
  });

  it("preserves order across a long burst", async () => {
    const chain = makeWriteChain<number>();
    const done: number[] = [];
    const runs = Array.from({ length: 20 }, (_, i) =>
      chain.run(1, async () => {
        // Interleave microtasks so an unserialized impl would scramble.
        await Promise.resolve();
        done.push(i);
      }),
    );
    await Promise.all(runs);
    expect(done).toEqual(Array.from({ length: 20 }, (_, i) => i));
  });

  it("drops a key's entry once its chain drains", async () => {
    const chain = makeWriteChain<number>();
    await chain.run(1, () => Promise.resolve());
    await tick();
    expect(chain.pendingKeys()).toBe(0);
  });
});
