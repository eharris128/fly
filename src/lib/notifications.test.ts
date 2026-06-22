import { describe, it, expect } from "vitest";
import {
  addNotification,
  markRead,
  markReadForLeaves,
  markAllRead,
  clear,
  clearAll,
  pruneToLeaves,
  unreadByLeaf,
  unreadTotal,
  newestUnread,
  toPersisted,
  relativeTime,
  coerceNotifications,
  type IngestedNotification,
  type Notification,
} from "./notifications";

function ev(over: Partial<IngestedNotification> = {}): IngestedNotification {
  return {
    id: 1,
    leafKey: "leaf-1",
    reason: "permission",
    title: "needs you",
    body: "details",
    ts: 1000,
    read: false,
    ...over,
  };
}

describe("addNotification", () => {
  it("honors the backend read-at-birth bit", () => {
    const unread = addNotification([], ev({ id: 1, read: false }));
    const read = addNotification([], ev({ id: 2, read: true }));
    expect(unread[0].state).toBe("unread");
    expect(read[0].state).toBe("read");
    // The read one contributes nothing to the unread count.
    expect(unreadTotal(read)).toBe(0);
  });

  it("dedupes by id (idempotent re-delivery)", () => {
    let list = addNotification([], ev({ id: 7 }));
    list = addNotification(list, ev({ id: 7, title: "again" }));
    expect(list).toHaveLength(1);
    expect(list[0].title).toBe("needs you");
  });
});

describe("read transitions", () => {
  it("markReadForLeaves flips a tab's leaves, leaving others", () => {
    let list = addNotification([], ev({ id: 1, leafKey: "leaf-1" }));
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-2" }));
    list = markReadForLeaves(list, ["leaf-1"]);
    expect(list.find((n) => n.id === 1)!.state).toBe("read");
    expect(list.find((n) => n.id === 2)!.state).toBe("unread");
  });

  it("re-raise re-unreads a leaf (a new unread entry appears)", () => {
    // leaf-1 has one read entry → zero unread.
    let list = addNotification([], ev({ id: 1, leafKey: "leaf-1", read: true }));
    expect(unreadByLeaf(list)["leaf-1"]).toBeUndefined();
    // It raises again (new id, unread) → leaf-1 is unread once more.
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-1", read: false }));
    expect(unreadByLeaf(list)["leaf-1"]).toBe(1);
  });

  it("markRead targets specific ids; markAllRead clears every unread", () => {
    let list = addNotification([], ev({ id: 1 }));
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-2" }));
    expect(unreadTotal(markRead(list, [1]))).toBe(1);
    expect(unreadTotal(markAllRead(list))).toBe(0);
  });
});

describe("clearing", () => {
  it("clear removes entries from counts", () => {
    let list = addNotification([], ev({ id: 1, leafKey: "leaf-1" }));
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-1" }));
    list = clear(list, [1]);
    expect(list).toHaveLength(1);
    expect(unreadByLeaf(list)["leaf-1"]).toBe(1);
  });

  it("clearAll empties the history", () => {
    expect(clearAll()).toEqual([]);
  });
});

describe("rollups", () => {
  it("unreadByLeaf aggregates per leaf with mixed states", () => {
    let list = addNotification([], ev({ id: 1, leafKey: "leaf-1" }));
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-1" }));
    list = addNotification(list, ev({ id: 3, leafKey: "leaf-2", read: true }));
    list = addNotification(list, ev({ id: 4, leafKey: "leaf-2" }));
    expect(unreadByLeaf(list)).toEqual({ "leaf-1": 2, "leaf-2": 1 });
    expect(unreadTotal(list)).toBe(3);
  });

  it("newestUnread returns the latest unread by ts, null when none", () => {
    let list = addNotification([], ev({ id: 1, ts: 100 }));
    list = addNotification(list, ev({ id: 2, ts: 300, leafKey: "leaf-2" }));
    list = addNotification(list, ev({ id: 3, ts: 200, leafKey: "leaf-3" }));
    expect(newestUnread(list)!.id).toBe(2);
    expect(newestUnread(markAllRead(list))).toBeNull();
  });
});

describe("pruning orphans on deletion", () => {
  it("drops notifications whose leaf is gone", () => {
    let list = addNotification([], ev({ id: 1, leafKey: "leaf-1" }));
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-2" }));
    const pruned = pruneToLeaves(list, new Set(["leaf-2"]));
    expect(pruned).toHaveLength(1);
    expect(pruned[0].leafKey).toBe("leaf-2");
    // No leaked unread, no jump dead-end.
    expect(unreadTotal(pruneToLeaves(list, new Set()))).toBe(0);
  });
});

describe("persistence + restore", () => {
  it("toPersisted keeps leafKey/title and drops bodies unless saveScrollback", () => {
    const list = addNotification([], ev({ id: 1, body: "secret output" }));
    const meta = toPersisted(list, false);
    expect(meta[0].body).toBeNull();
    expect(meta[0].title).toBe("needs you");
    expect(meta[0].leafKey).toBe("leaf-1");
    expect(toPersisted(list, true)[0].body).toBe("secret output");
  });

  it("round-trips through coerceNotifications and keeps state", () => {
    let list = addNotification([], ev({ id: 1, read: true }));
    list = addNotification(list, ev({ id: 2, leafKey: "leaf-2", read: false }));
    const restored = coerceNotifications(
      JSON.parse(JSON.stringify(toPersisted(list, true))),
    );
    expect(restored).toHaveLength(2);
    // Restore is NOT auto-read: the unread one stays unread (missed-notification
    // value preserved); the read one stays read.
    expect(restored.find((n) => n.id === 1)!.state).toBe("read");
    expect(restored.find((n) => n.id === 2)!.state).toBe("unread");
    // leafKey survives the round-trip (resolves after paneId reassignment).
    expect(restored.find((n) => n.id === 2)!.leafKey).toBe("leaf-2");
  });

  it("relativeTime renders compact buckets", () => {
    const now = 1_000_000;
    expect(relativeTime(now, now)).toBe("now");
    expect(relativeTime(now - 30_000, now)).toBe("30s ago");
    expect(relativeTime(now - 5 * 60_000, now)).toBe("5m ago");
    expect(relativeTime(now - 3 * 3_600_000, now)).toBe("3h ago");
    expect(relativeTime(now - 2 * 86_400_000, now)).toBe("2d ago");
    expect(relativeTime(now + 5000, now)).toBe("now"); // clamps future
  });

  it("a fresh uniquely-id'd notification appends alongside low-id restored entries", () => {
    // Regression guard for the restart id-collision bug: restored history from a
    // prior session carries small ids (0,1); the backend now mints new ids from
    // wall-clock, so a genuinely-new event never collides and must append (not
    // be swallowed by the id dedup). The dedup still only fires on a true
    // same-id repeat.
    const restored = coerceNotifications([
      { id: 0, leafKey: "leaf-1", reason: "permission", ts: 100, state: "read" },
      { id: 1, leafKey: "leaf-1", reason: "finished", ts: 200, state: "read" },
    ]);
    const next = addNotification(
      restored,
      ev({ id: 1_700_000_000_123, leafKey: "leaf-1", ts: 999 }),
    );
    expect(next).toHaveLength(3);
    expect(next.some((n) => n.id === 1_700_000_000_123)).toBe(true);
  });

  it("coerceNotifications tolerates absence and drops malformed entries", () => {
    expect(coerceNotifications(undefined)).toEqual([]);
    expect(coerceNotifications("nope")).toEqual([]);
    const mixed = [
      { id: 1, leafKey: "leaf-1", reason: "permission", ts: 5, state: "unread" },
      { id: "bad", leafKey: "leaf-2", reason: "permission", ts: 5 }, // bad id
      { id: 3, leafKey: "leaf-3", reason: "bogus", ts: 5 }, // bad reason
      { leafKey: "leaf-4", reason: "error", ts: 5 }, // missing id
    ];
    const out = coerceNotifications(mixed);
    expect(out).toHaveLength(1);
    expect(out[0].id).toBe(1);
  });
});
