// Per-key async serializer (poll-batching plan KTD5/R5). Built for
// `ipc.ts::ptyWrite`: the backend `pty_write` command is async, so two
// in-flight invokes can complete out of order across runtime workers —
// per-pane keystroke order is pinned here instead, by starting each key's
// task only after that key's previous task settles. Pure and framework-free
// (the repo's lib convention) so ordering is testable without mocking Tauri.

/**
 * A per-key task serializer. `run(key, task)` starts `task` only after every
 * previously-run task for `key` has settled, and returns `task`'s own promise
 * (so a failure rejects its caller — but never wedges the chain: the stored
 * tail swallows settlement state). A key's entry self-removes once its chain
 * drains, so the map only holds keys with work in flight.
 */
export interface WriteChain<K> {
  run<T>(key: K, task: () => Promise<T>): Promise<T>;
  /** Number of keys with an unsettled chain (test/inspection surface). */
  pendingKeys(): number;
}

export function makeWriteChain<K>(): WriteChain<K> {
  const tails = new Map<K, Promise<void>>();
  return {
    run<T>(key: K, task: () => Promise<T>): Promise<T> {
      const prev = tails.get(key) ?? Promise.resolve();
      const next = prev.then(task);
      const tail = next.then(
        () => undefined,
        () => undefined,
      );
      tails.set(key, tail);
      void tail.then(() => {
        if (tails.get(key) === tail) tails.delete(key);
      });
      return next;
    },
    pendingKeys(): number {
      return tails.size;
    },
  };
}
