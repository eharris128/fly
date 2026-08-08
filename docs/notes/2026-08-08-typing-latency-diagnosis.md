# Typing-latency diagnosis — 2026-08-08

Live measurement of the persistent typing delay, taken against the installed
release build (built 2026-08-08 17:52, includes the T1 coalesce fix
`baf4f6b`) while the app was in the reported laggy state: ~10 live panes,
four of them Claude agents emitting spinner-cadence output, no heavy output
flood. Conclusion up front: **T1 fixed what it targeted (IPC message
volume), and the remaining lag has two stacked causes — the audit's T4
(visible-pane DOM render cost) saturating the webview main thread, and the
audit's T2 (sync per-pane poll storm) stalling the app main thread, where
`pty_write` also runs.** See
`docs/notes/2026-07-23-performance-audit-follow-ups.md` for the audit
entries; the T2 fix plan is
`docs/plans/2026-08-08-003-perf-pane-status-poll-batching-plan.md`.

## Symptom history

- Multi-second input lag while agents run; first attacked 2026-08-04 by T1
  (`baf4f6b` — per-pane output coalescing + watermark tightening), whose
  hypothesis was per-read `eval()` cost on the webview main thread.
- 2026-08-08: user reports the delay unchanged in day-to-day use. The
  installed binary was verified to contain T1 (rebuilt/installed the same
  day), so this is not a stale-build artifact.

## Measurements (2026-08-08 ~19:00)

All numbers from `/proc` sampling of the live processes: fly app pid 6502,
WebKitWebProcess pid 6540, uptime ~1h02m at sample time.

1. **Lifetime CPU averages:** WebKitWebProcess 44.7% of a core, fly 18.0%.
   Both concentrate the burn on their **main threads**: the webview main
   thread had 156,997 of its process's ~160k ticks (~26 of 28 CPU-minutes);
   fly's main thread 60,310 ticks (~10 min) with every named worker thread
   (`fly-coal-*`, `fly-pty-*`, tokio, feed, sweep) near zero.
2. **100 ms-resolution trace (6 s):** the webview main thread sits at
   ~90–110% of a core **continuously**; the fly main thread idles at 0–40%
   with **80–110% bursts lasting ~200–300 ms every ~1.5 s**.
3. **Output volume is trivial:** all `fly-pty-*` + `fly-coal-*` threads
   together burned **0.3%** of a core over a 10 s window. T1's target — byte
   volume and message count — is genuinely fixed.
4. **Flush-rate ↔ webview-wakeup correlation (the key finding):** four
   panes' coalescer threads woke 7–16 times/s each, **~46 flushes/s
   total**; the webview main thread's voluntary context switches ran at
   **~48/s** while it burned ~100% CPU. One wake per flush, thread
   saturated ⇒ **each coalesced spinner flush costs ~20 ms of webview
   main-thread work**. That is render cost (xterm DOM-renderer row
   rebuilds), not parse or transport cost — the flushes are tiny.
5. All four streaming panes flushed faster than the 4/s hidden-pane
   deadline cap, so all four were in the **visible** set — consistent with
   a multi-split layout of agents whose spinner repaints each re-render
   rows through the DOM renderer.

## Diagnosis

Typing traverses three queues; two are congested:

- **Webview main thread (saturated, continuous):** keydown JS → xterm
  `onData` → `invoke` serialization all wait behind ~46 render jobs/s ×
  ~20 ms each. This is the audit's **T4** ceiling ("DOM is the ceiling") —
  T1 reduced *how often* renders happen, not *what each costs*, and at
  spinner cadence the coalescer can't merge further (writes arrive ~100 ms
  apart, far beyond the 4 ms visible deadline).
- **App (GTK) main thread (stalls every 1.5 s):** the audit's **T2** poll
  storm — `pane_activity` (full `/proc` walk per pane), `pane_session_id`
  (project-dir readdir + per-entry stat per pane), `pane_cwd`,
  `pane_command` — are all **sync Tauri commands and therefore run on the
  GTK main thread** (measured as the 200–300 ms bursts). **`pty_write` is
  also a sync command on that same thread** (`pty/mod.rs::pty_write`), so a
  keystroke that survives the webview queue can then stall up to ~300 ms
  behind a poll burst. Additionally the nudge effect polls
  `pane_activity(focusedPane)` at **1 Hz whenever the dashboard is closed**
  (`App.svelte` nudge `$effect`) — a full `/proc` walk per second on the
  main thread precisely while the user is typing into a focused pane.
- The backend PTY write path itself is clean (writer handles cloned out of
  the registry lock, KTD13) — the congestion is thread scheduling, not
  locks.

Not implicated in typing latency: the audit's un-owned #1 (feed resolver
full transcript re-read per 1.5 s) burns feed-thread CPU (part of fly's 18%
average) but runs off both main threads.

## Consequences / next steps

1. **T2 fix first** (plan
   `2026-08-08-003-perf-pane-status-poll-batching-plan.md`): one batched
   async status command per tick, one `/proc` snapshot, session-id scan
   TTL'd, `pty_write` off the main thread. Kills the 300 ms hitches and the
   1 Hz nudge walk; cheap and render-risk-free.
2. **T4 second**: WebGL addon on visible panes, dispose-on-hide (bounded
   contexts, DOM fallback, no unmount). Attacks the ~20 ms/flush render
   cost — the continuous saturation. Needs the deferred WebKitGTK live
   validation (CLAUDE.md capture recipe). **Landed 2026-08-08** —
   `lib/renderer.ts` + `Terminal.svelte`, default `renderer` flipped to
   `auto`; see the T4 entry in
   `docs/notes/2026-07-23-performance-audit-follow-ups.md`.
3. Expectation setting: after T2 alone, typing should stop hitching in
   bursts but the webview can still saturate while ≥3–4 agent spinners are
   *visible*; full relief under many visible streaming panes is T4's job.

## Post-T2 re-measurement (2026-08-08 21:28, new build live)

The poll-batching fix restarted into production and re-measured with the same
protocol, agents streaming:

- **fly main thread: bursts gone.** Average 5.5% of a core, peak 30% per
  100 ms sample, no saturation bursts at the poll cadence (was 80–110% ×
  200–300 ms every 1.5 s). R8 met — keystroke writes no longer queue behind
  poll storms.
- **Webview main thread: ~40% average** with visible streaming agents, idle
  stretches at 0% (was pinned ~100% continuously). What remains tracks
  output bursts — the T4 DOM-render cost, unchanged by design; WebGL is the
  next step.
- PTY/coalescer threads 0.2% — unchanged, as expected.

## Post-T4 live validation (2026-08-08 ~22:30, dev flavor on WebKitGTK)

T4 (WebGL on visible panes, dispose-on-hide — `lib/renderer.ts` +
`Terminal.svelte`, default `renderer` now `auto`) validated live against the
dev flavor under X11, three visible panes, one emitting a 33-lines/s
spinner-cadence loop for 10 s. Webview main thread sampled at 100 ms:

| renderer      | avg CPU | peak | samples >50% |
|---------------|---------|------|--------------|
| auto (run 1)  | 47.9%   | 100% | 19%          |
| auto (run 2)  | 45.8%   | 100% | 14%          |
| dom (pinned)  | 60.3%   |  90% | **75%**      |

The busy-fraction is the discriminator: DOM grinds >50% of a core for ~¾ of
the load duration (~18 ms/flush — reproducing the ~20 ms diagnosis figure);
WebGL stays under 50% for ~85% of it. This is the protocol's acceptance
signal ("the CPU% must not [saturate]"). Render correctness also validated:
three simultaneous GL contexts, scrollback scrolling while streaming, and the
critical dispose→re-attach reveal (tab away/back) — content intact, scroll
position preserved, **no blanking** (the historical failure the eviction was
designed around).

## Re-measurement protocol (for validating the fixes)

Identify pids (`ps -eo pid,comm | grep -i 'fly\|WebKit'`), then:

- Main-thread burst profile: sample `utime+stime` of
  `/proc/<pid>/task/<pid>/stat` at 100 ms for ~6 s (fly main thread should
  show no ≥100 ms 100% bursts after T2).
- Flush rate: `voluntary_ctxt_switches` deltas of `fly-coal-*` threads
  (`/proc/<fly>/task/*/status`) over ~5 s.
- Webview wakeups: same counter on the webview main thread; compare to the
  flush total. The ~1:1 wake-per-flush correlation is the T4 signature; it
  should survive T2 and disappear only when WebGL lands (renders get cheap,
  the thread stops saturating — the wake rate may stay, the CPU% must not).
