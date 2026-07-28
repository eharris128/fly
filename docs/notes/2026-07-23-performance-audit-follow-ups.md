# Performance audit follow-ups — 2026-07-23

Deferred tasks from the 2026-07-23 four-agent performance audit (backend
streaming path, feed/transcript subsystem, Svelte/xterm frontend,
automations/background work). Every finding below was verified by reading the
code at audit time; line numbers are as of `97dd705` and will drift — the
`file::function` anchors are the durable reference.

**Not listed here:** the audit's #1 finding — the feed resolver's O(file)
transcript re-read/re-parse per 1.5s tick while an agent streams
(`session/transcript.rs::transcript_io` full-read + two forward full-parses;
fix = bounded backward tail read + merged single pass) and the coupled
`feed/io.rs` deep-clone of `ResolvedIo` per frame (fix = `Arc<ResolvedIo>` or
a stamps-only accessor) — which was to be pursued immediately. T13 below is a
rider that falls out of that work.

**Status as of 2026-07-28: that #1 fix did not land.** `transcript_io`
(`session/transcript.rs`) still full-reads and runs three forward parses per
call, and `feed/io.rs` still deep-clones. It is un-owned work, not
in-progress work — treat it as the top item of this list rather than as
already-handled.

Priority tiers: **P1** = measurable steady-state cost, worth scheduling.
**P2** = real but situational. **P3** = polish, batch opportunistically.

---

## P1 — steady-state costs

### T1 — Coalesce PTY output chunks before the Tauri channel
- **Where:** `stream/mod.rs` (the channel sink in `spawn_pane`, ~:147).
- **What today:** the read thread calls `channel.send(InvokeResponseBody::Raw(...))`
  once per PTY `read()`. Verified in the vendored tauri 2.11.3 `ipc/channel.rs`:
  chunks **< 1024 bytes** are serialized as a decimal JSON number array
  (~3.7× expansion) embedded in an `eval()` script and parsed on the webview
  main thread; chunks ≥ 1024 ride raw but still cost one eval + one
  custom-protocol fetch round trip + two ops on a global
  `ChannelDataIpcQueue` mutex shared across all panes. Interactive TUI output
  (spinners, Ink repaints) is exactly many small writes.
- **Fix:** per-pane micro-batcher in fly's sink — flush on ~2–4 ms deadline or
  32–64 KiB, whichever first (small forwarder thread, or try-drain of
  immediately-available data after each read). Ordering preserved (one
  channel). Collapses spinner storms to a few messages/sec and pushes most
  traffic onto the ≥ 1 KiB raw path.
- **Caveat:** this contradicts the foundation plan's KTD3 / CLAUDE.md claim
  "raw bytes end-to-end, no transcoding". ~~re-verify the vendored-source
  reading first, then correct that doc text in the same change.~~
  **Done 2026-07-28** — re-verified against the vendored source and corrected
  at all four sites (foundation-plan KTD3 addendum + U4, `CLAUDE.md`,
  `stream/mod.rs`). The doc fix no longer blocks this task.
- **Re-measured 2026-07-28** (the magnitude the original entry left open):
  - Expansion of the JSON-number-array form on real captured Claude Code
    renders (`tests/fixtures/screen/*.raw`, 67 KB): **3.39×**, not ~3.7×.
  - The small path is not an edge case. A PTY read loop mirroring
    `pty/pane.rs` saw **60/60 reads under 1024 bytes** (median 49 B) during
    20 Hz spinner repaints — i.e. *every* interactive frame transcodes and
    evals. Under a 512 KiB flood, 92% of bytes rode the raw ≥1 KiB path, so
    the batcher's win is concentrated in the idle/thinking case, not the
    flood case the entry originally framed.
  - Also measured: `READ_BUF`'s 64 KiB never fills. A 1 MiB single `write()`
    came back as reads of median 2048 / max 8193 bytes (Linux 6.8) — the
    kernel PTY buffer, not fly's buffer, sets the chunk size. Batching must
    therefore happen in fly's sink; enlarging `READ_BUF` cannot help.
- **Impact:** high (core-surface throughput + latency). Confidence: mechanism
  and magnitude both high as of the 2026-07-28 measurements; end-to-end
  user-visible latency gain still unmeasured.

### T2 — Batch the per-pane 1.5s polls; kill the /proc and readdir storms
Three audits converged here; this is the idle-power story. One task, three
parts (they share the same plumbing):
- **(a) Shared /proc snapshot for activity.** `pty/mod.rs::agent_task_count`
  calls `cwd::read_proc_table()` — a full /proc walk (open/read/close of every
  process's `stat`, ~400+ procs on this box) — **per agent pane per 1.5s tick**
  whenever the dashboard is open or the feed is enabled (i.e. permanently for
  a feed user; the worker ticker deliberately survives backgrounding).
  Fix: a batched `panes_activity(Vec<PaneId>)` command reading the table once
  per tick; `count_background_task_groups` is already pure over a table.
  Build `descendants_in_table`'s pid map once per snapshot too.
- **(b) Project-dir scan short-circuit.** `pty/mod.rs::pane_session_id` →
  `session/transcript.rs::active_session_for_cwd` → `read_project_entries`
  does a `read_dir` of `~/.claude/projects/<encoded-cwd>/` plus a `metadata()`
  stat **per entry, per pane, per 1.5s** (a busy dir has 100s of transcripts).
  Session rotation is rare and the SessionStart hook is the precise fast path;
  the poll is only the version-skew fallback. Fix: stat the project dir's own
  mtime and skip the entry scan when unchanged (verify create/append semantics
  bump the dir mtime first), and/or drop this capture to ~5s cadence.
- **(c) One invoke per tick, not 3–4 per pane.** `App.svelte` runs two
  unsynchronized 1.5s pollers (`refreshCwds` on a main-thread interval;
  `refreshAgents` on the worker ticker) issuing `pane_cwd` + `pane_command` +
  `pane_session_id` (+ `pane_activity`) per pane — ~60–80 invokes/1.5s at 20
  panes, each a JSON + WebKit postMessage round trip. `pane_command` polls
  forever for shell panes that will never become agents, and serially (its
  siblings are parallel). The nudge effect double-polls the focused pane at
  1 Hz. Fix: fold into one batched `panes_status(ids)` command on one clock;
  gate session-id/command capture to panes whose argv identified an agent.
- **Bonus (same restructure):** `pty/mod.rs::foreground_pid` does the
  tcgetpgrp ioctl while holding the pane-registry lock, against the module's
  own KTD13 no-syscall-under-lock discipline — fix in passing.
- **Impact:** medium-high (this is most of fly's idle CPU/power draw).
  Confidence: high.

### T3 — Stop the automations sweep's no-op flush; stop cloning to peek the gate
- **Where:** `automations/mod.rs::sweep_once` + `automations/store.rs::mutate`.
- **What today:** every 10s tick calls `store.mutate(...)` unconditionally;
  `mutate` always flushes — full-map `to_vec_pretty`, temp write, `sync_all`,
  rename, parent-dir fsync — **two fsyncs every 10s even with an empty store**,
  while holding the store mutex (so dashboard/feed/CLI reads can stall behind
  a multi-ms fsync). The store doc's "spurious full-document write is
  harmless" was written for spurious *mutations*; the sweep made it a timer.
  Separately, with the usage gate on (default), each tick deep-clones the
  whole `Config` (`config.lock().get()`) **and** the whole automation map
  (`store.snapshot()`, incl. all run rows' 8 KiB output tails) just to compute
  the boolean `agent_claim_possible`.
- **Fix:** dirty-gated flush (the mutate closure already tracks
  touched/changed — return the dirty bit and skip the write when clean, or
  pre-check due-ness before entering the mutating phase); add read-only
  closure accessors (`Store::with(|map| …)`, `ConfigStore::read_with(…)`) for
  the gate peek. No lock-discipline change.
- **Impact:** high for the flush (perpetual disk wakes + lock contention),
  low-medium for the clones. Confidence: high.

---

## P2 — real but situational

### T4 — WebGL renderer for the visible pane (build the deferred KTD6 eviction)
- **Where:** `lib/Terminal.svelte` (renderer selection); default renderer is
  DOM (`config/schema.rs`, `renderer`).
- **What today:** the visible pane doing heavy agent output renders through
  xterm's slowest path (per-refresh row-DOM rebuild on the WebKitGTK main
  thread). WebGL exists but only as an all-or-nothing config opt-in, because
  multiple live GL contexts blank inactive panes on WebKitGTK — the
  foundation plan's KTD6 per-pane context-eviction policy was never built.
- **Fix:** load `WebglAddon` only on visible panes (or just the focused one)
  and `addon.dispose()` on hide — addon disposal is not a terminal unmount
  (xterm falls back to DOM transparently, no agent respawn), so the
  never-unmount invariant holds and live contexts stay bounded at ~1.
- **Caveat:** needs the live WebKitGTK validation the original plan deferred
  (single evicted context stability). Use the CLAUDE.md capture recipe.
- **Impact:** medium-high on render throughput. Confidence: high that DOM is
  the ceiling; medium on WebKitGTK stability.

### T5 — Absolute timestamps on the activity wire so idle can quiesce
- **Where:** `PaneActivity` in `ipc.ts` / its backend shape
  (`pty/mod.rs::pane_activity`); consumers `App.svelte::refreshAgents`,
  `lib/home.ts`, `lib/feed.ts::buildFeedPayload`.
- **What today:** the wire carries relative ages (`workingForMs`,
  `lastOutputAgoMs`) that change every poll **by construction**, so
  `agentByLeaf` is wholesale-replaced per tick, `homeModel` recomputes,
  HomeView re-renders, and `buildFeedPayload` + a publish IPC run — even with
  every agent idle. No equality guard can help as shaped. (The backend SSE
  dedup keeps external cost down; the webview churn is structural.)
- **Fix:** send absolute stamps (`workingSinceMs`, `lastOutputAtMs`), derive
  ages at render time from HomeView's existing 1s ticker; then an idle roster
  is bit-stable and a shallow-equality guard can skip the reassignment.
- **Impact:** low-medium (CPU when idle with feed/dashboard on).
  Confidence: high.

### T6 — Throttle the divider-drag / resize cascade (+ pointercancel bug)
- **Where:** `App.svelte::startDrag` (~:1480); the visible-panes `$effect`;
  `lib/Terminal.svelte` ResizeObserver → `fit()`.
- **What today:** every pointermove runs `setRatio` + a full workspaces
  reassignment → every derived recomputes (allPanes, sidebarWorkspaces,
  notificationEntries with per-notification full-tree `locateLeaf`, palette,
  rects, homeModel) → fresh `visibleLeafKeys` array fires a `setVisiblePanes`
  IPC per move → slot style changes fire each visible pane's ResizeObserver →
  `fit.fit()` reflows a 10k-line xterm buffer + `ptyResize` IPC on grid
  change. Plus a forced layout from `getBoundingClientRect()` per move.
  Window resize takes the same cascade.
- **Fix:** rAF-throttle pointermove → one tree update per frame; debounce
  `fit()`/`ptyResize` ~50–100 ms trailing during interactive resize (standard
  terminal practice); make the visible-panes effect compare content before
  pushing. Also a genuine small bug: `startDrag` registers
  pointermove/pointerup but **not `pointercancel`** — a cancelled drag leaves
  the per-move cascade live until some pointerup arrives (Sidebar handles it;
  App should too).
- **Impact:** medium during interaction, zero at rest. Confidence: high.

---

## P3 — polish, batch opportunistically

### T7 — mtime-keyed cache for `resume.json` reads in the feed resolver
`feed/io.rs::ReplyResolver::transcript_path` and the fallback chain
(`feed/fallback.rs`) each call `resume::read_records` — the whole store is
read + parsed **twice per resolve**, × agents × frames. Fix with an
mtime-gated cache, **not** a pure in-memory map: out-of-band writes to
`resume.json` being picked up fresh is relied on (dev-flavor live-validation
techniques memory).

### T8 — TTL cache for the `~/.claude/sessions/` scan
`session/livestate.rs::waiting_state` read_dirs + parses every `<pid>.json`
until a sessionId matches. Its own comment claims "never per frame for
settled agents", but since the fix-feed-question-detection-gaps gate
widening the resolver reaches it whenever the transcript yields no pending
question — which on Claude ≥ 2.1.206 is always at ask time and for every
settled agent. A ~1s-TTL parsed-scan cache shared across leaves makes one
scan answer all agents in a frame pass. Fix the stale comment either way.

### T9 — `tail_seq()` accessor to skip the 64 KiB ring copy on cache hits
`feed/fallback.rs::parsed_screen` fetches the full ring snapshot before
consulting its seq-keyed parse cache (it needs the seq), so a pane parked on
a dialog — frozen seq, the exact cache-hit case — still pays a 64 KiB copy
under the pane-registry lock (`pty/mod.rs::screen_tail_by_leaf`) every
frame. Add a cheap `(seq, last_write_ms, rows, cols)` accessor; fetch bytes
only on miss.

### T10 — `persist()` should read `cwdByLeaf`, not re-probe serially
`App.svelte::persist` awaits `paneCwd` **sequentially** per pane inside the
save (20 panes = 20 serial IPC round trips per debounced save), duplicating
what `refreshCwds` refreshed ≤ 1.5s ago. Read the map; keep the live probe
only for the on-close save (which already exists and genuinely needs it).
`Promise.all` is the floor if the probe stays.

### T11 — One App-level listener for `pane://attention` / `pane://exit`
Every Terminal registers its own two `listen()`s filtering by paneId, so
every attention event runs N handlers and each mount costs two registration
round trips. App already owns `leafByPaneId`; dispatch centrally, O(1)
(the `onNotificationAdded` pattern). Cleanup is correct today — overhead,
not a leak.

### T12 — Two-phase pane teardown at quit
`pty/mod.rs::close_all` → `Pane::teardown` is serial: SIGHUP →
200 ms grace → SIGKILL, per pane. N panes ignoring SIGHUP (agents mid-work)
= N × 200 ms added to quit. Phase 1: mark stopping + SIGHUP all; phase 2:
wait/escalate/join. Worst case ~200 ms total.

### T13 — Rider on the in-flight #1 tail-read: handoff pick-list
`session/handoff.rs::list_candidates_in_root` → `session_turn_summary`
full-reads + line-parses **every** transcript in a project dir on picker
open (user-triggered, so P3). The last turn + snippet are tail-local — the
bounded backward tail read shipped for #1 should be reused here; check this
off when that lands.

### T14 — Grab-bag (each trivial, none urgent)
- `usage/mod.rs`: fresh `reqwest::Client` (pool + TLS) per fetch — `OnceLock`
  it if usage fetches ever get more frequent; rare today.
- `automations/redact.rs` scrub internals allocate per word
  (`to_string`/lowercase copies, per-line Vecs) and `feed/io.rs::clean` walks
  the string twice — `Cow` pass-through for the no-match prose case +
  `eq_ignore_ascii_case` + single-pass truncate. Only worth doing after the
  #1 tail-read lands (it's gated to cache miss/capture time).
- Unbounded-but-small per-leaf maps never evict dead leaves:
  `ReplyResolver.cache`, `FallbackResolver.parse_cache`, the feed server's
  answered latch, `feed/pending.rs::PendingSignals`, and `App.svelte`'s
  `paneRefs` (the one map excluded from `prunePaneIdMaps`). A prune alongside
  `resume::retain_at` / leaf close would tidy all of them.
- Automation-tab xterm scrollback: ephemeral automation/sink tabs hold the
  full 10k-line buffer like every never-unmounted pane (~10–25 MB each when
  filled); pass a smaller scrollback option at mount for those tabs.

---

## Verified efficient — do not re-flag in future audits

- **PTY read-thread hot loop** (`pty/pane.rs`): per-chunk cost after the sink
  is one Relaxed-atomic activity update, one bounded ring memcpy, one vDSO
  clock read; never touches registry/attention locks or Tauri state; bytes
  sent before bookkeeping.
- **Backpressure** is lossless and zero-CPU: condvar-parked read thread,
  kernel PTY buffer holds the child, 2 MiB/512 KiB frontend watermarks via
  xterm write-completion callbacks.
- **Hidden panes are cheap**: `display:none` + xterm 5.5's
  IntersectionObserver pauses rendering (VT-parse cost only, necessary for
  buffer state); ResizeObservers early-out at zero size; reveal costs one
  `fit()`.
- **Feed SSE core**: publish dedups identical rosters, readers park on a
  condvar, frames carry roster + stamps only, idle feed costs nothing;
  screen parse memoized by ring seq, vte grid capped at 200 rows;
  `AskRegistry` fully event-driven, capped.
- **No regex anywhere in the audited request paths** — all scanning
  (redact, snippet sanitize, iso8601, screen) is hand-rolled linear.
- **Automations scheduling core**: due-ness is a stored-timestamp compare;
  cron math runs only on claim/skip/create; the sweep thread is one condvar
  `wait_timeout` per 10s with instant shutdown wake.
- **Usage**: dashboard fetch on open only (KTD-C holds); gate TTL-cached 60s
  (errors too), consulted only on claimable agent ticks.
- **Alerts**: event-driven, O_APPEND single writes, one-time startup
  truncate, independent mutex.
- **Resume store writes**: change-gated by the frontend; fsync + unique-temp
  + rename per upsert is deliberate durability (audit-remediation U5).
- **Headless kill sweep**: the /proc descendant snapshot runs only at kill
  time, with start_time pinning against pid reuse.
- **`TokenRegistry::validate`'s linear constant-time compare** is deliberate
  timing safety at trivially low rates — not a perf bug.
- **Reactivity structure**: keyed flat leaves (no remount on split/resize/
  switch); the pure view-models (`layout`, `workspaces`, `home`, `feed`,
  `automation-panes`) are O(leaves) with no quadratic passes.
