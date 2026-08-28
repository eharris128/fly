# tmux 3.6 substrate spike — results — 2026-08-28

Plan: `docs/plans/2026-08-28-001-spike-tmux-36-substrate-validation-plan.md`.
Box: tmux **3.6** (`3.6a-2ubuntu0.1`), kernel 7.0.0-30, `/usr/bin/cat` =
uutils coreutils 0.8.0 (`coreutils-from-uutils`; GNU 9.7 present only as
`/usr/bin/gnucat`). Binary: `core/target/debug/fly` at `c517706`, no code
change. Everything below ran on scratch flavors/servers (`flylatt`, `flylatp`,
`flyspike-*`, `fly*-<pid>`); the installed release was never touched.

## U0 — baseline (executed 2026-08-28)

### U0.1 `substrate_live` on tmux 3.6a — 3 of 6 fail, all in the exit family

`cargo test --offline --test substrate_live -- --ignored --test-threads=1`
(first ever run on 3.6; last green on 3.4):

| test | result |
|---|---|
| `tmux_pane_output_input_resize_teardown_roundtrip` | ok |
| `server_env_is_scrubbed_of_claude_markers` | ok |
| `ephemeral_pane_is_killed_at_quit_not_detached` | ok |
| `tmux_pane_child_exit_surfaces_exited_state` | **FAILED** — "exit should surface via pane_dead within 10s" |
| `pane_died_hook_reports_exit_over_the_socket` | **FAILED** — "hook-driven exit should surface without any poll: Timeout" |
| `restart_roundtrip_detach_adopt_preserves_session_and_hooks` | **FAILED** — "hook-driven exit reaches instance B: Timeout" |

**Not a tmux 3.6 delta.** Ruled out in order: a harness artifact (`sleep`
children survive here, both bare and inside a scratch pane); tmux semantics
(on a scratch 3.6 server `remain-on-exit` sticks whether set with `-t
session`, `-w`, or `-gw`, `#{pane_dead}`/`#{pane_dead_status}` report `1 7`
within a second, and a session-scoped `set-hook -t <s> pane-died` fires);
fly's poll (strace: the test issues `list-panes -a -F '#{session_name}
#{pane_dead} #{pane_dead_status}'` 40×, and each client `writev`s
`fly-…-exit-leaf 1 7\n` back to fly; the executor's exact invocation shape —
stdin `/dev/null`, stdout a pipe — returns the line by hand too).

The actual failure, from a full-syscall strace of the FIFO read thread
(`pty/pane.rs::tmux_read_loop`):

```
openat("…/pane-1.pipe", O_RDWR|O_CLOEXEC) = 8
fcntl(8, F_SETFL, O_RDWR|O_NONBLOCK)      = 0
poll([{fd=8, POLLIN}], 1, 500)            = 0 (Timeout)     ← 500 ms later
close(8 <unfinished …>                                       ← forced_exit seen; break 'outer; drop(reader)
<… close resumed>)                        = 0               ← 10.2 s LATER, when teardown killed the server
exit(0)
```

So `force_dead` *did* run (the poll chain works on 3.6), the thread broke
out of its loop as designed, and then **`close()` on the FIFO blocked for
10 s** — it reached `on_exit` only after the test had given up. All three
failures are this one hang: the hook and restart tests are the same
`close()` behind the same lock.

### U0.2 the mechanism (scratch server, no fly): splice holds the pipe lock

The `pipe-pane` consumer is `cat > '<fifo>'`. uutils `cat` copies with
`splice(0 → 1, 1 MiB)` from tmux's socketpair straight into the FIFO.
Linux's `splice_file_to_pipe()` takes the **destination pipe's mutex** and
then calls the socket's `splice_read`, which **sleeps waiting for socket data
while still holding that mutex**. Every other operation on the FIFO that
needs the pipe lock — fly's `read()` (even `O_NONBLOCK`) and `close()` —
blocks *uninterruptibly* (D-state, `wchan=anon_pipe_read`) until the next
byte arrives on the socket, i.e. until the pane produces more output. The
reader routinely loses the re-lock race to `cat`'s next `splice` call, so a
key's echo is released by the *next* keystroke (sometimes the one after).

Timed on a scratch server (pane running `cat`; reader opens the FIFO
`O_RDWR|O_NONBLOCK`, `poll`s, `read`s, then `close`s in a thread; a watchdog
sends `b` at +3.0 s and `c` at +6.0 s, kills the server at +8 s):

| consumer | `poll` readable | `read()` returned | `close()` returned | consumer wchan while idle |
|---|---|---|---|---|
| uutils `cat` | 229 ms | **6153 ms, `b'abc'`** (D-state until the 3rd key) | **8170 ms** (only when `cat` was killed) | — |
| GNU `cat` (`gnucat` 9.7) | 236 ms | 236 ms `b'a'` | 237 ms | — |
| `dd bs=65536 status=none` | 229 ms | 229 ms `b'a'` | 229 ms | `unix_stream_data_wait` (no pipe lock held) |

Earlier the same day, the pure output-side matrix (12 single-byte echoes,
consumer → FIFO, fly-shaped reader): uutils `cat` 5 reads `[3@255 3@616
2@863 3@1228 1@2708]` (last byte on pipe close); busybox `cat` same
clumping (sendfile/splice); GNU `cat`, `dd`, and a python `os.read/os.write`
loop 12/12 each ≤ 6 ms; uutils `cat` → *regular file* is fine (different
kernel path); an attached control-mode client changes nothing; under
`strace` the bug hides (timing). GNU-cat immunity is why the substrate
measured clean at build time.

**Consequence for the plan:** KTD1 stands and is sufficient — a read/write
consumer never holds the pipe lock across a wait. Q2 ("does 3.6 behave?")
collapses to a confirmation pass: nothing 3.6-specific was found; the
primitives fly uses behave as on 3.4 (also checked: the pipe-pane `cat`
outlives a dead pane on 3.6, as the U4 build-log scar says; sockets created
in one shell are reachable from another, so a watcher can run beside a
test). U3 shrinks to re-running the suite green after U1 plus the hook-path
exit latency measurement.

### U0.3 echo probe through `fly core` — before numbers (KTD2)

Isolated cores over their control sockets (`FLY_APP_NAME=flylatt` with
`{"substrate":"tmux"}`, `flylatp` with `pty`), pane spawned via `spawn_pane`
running `cat`, marked visible (`set_visible_panes` → 4 ms coalesce
deadline), keys down 0x03 / echoes up 0x02, 50 keys per gap, distinct key
char per run, 0.5 s + 2 s idle, then `C-u` (150 erase bytes) as "later
output". Debug binary; the 4 ms figure is the coalescer deadline, so debug vs
release is noise here.

| substrate | gap | echoed ≤ 500 ms | late in idle | withheld until later output | p50 | max |
|---|---|---|---|---|---|---|
| **pty** | 10 ms | 50/50 | 0 | 0 | 4.28 ms | 4.84 ms |
| pty | 30 ms | 50/50 | 0 | 0 | 4.26 ms | 4.85 ms |
| pty | 100 ms | 50/50 | 0 | 0 | 4.44 ms | 4.74 ms |
| pty | 300 ms | 50/50 | 0 | 0 | 4.31 ms | 4.81 ms |
| **tmux (cat consumer)** | 10 ms | 49/50 | 0 | 1 | **14.91 ms** | 25.92 ms |
| tmux | 30 ms | 50/50 | 0 | 0 | **35.27 ms** | 65.77 ms |
| tmux | 100 ms | 50/50 | 0 | 0 | **104.66 ms** | 205.73 ms |
| tmux | 300 ms | 48/50 | 0 | 2 | **304.58 ms** | 605.68 ms |

Read: on tmux, **echo latency ≈ the inter-key gap** (p50 = gap + ~5 ms, max
≈ 2 × gap) — each echo is released by the next keystroke — and the last
write of any burst is withheld until more output (the 150-byte `C-u` erase
was seen 0/150 in two runs and arrived as "stray bytes" during the next
run). That is the felt input lag, quantified. The gate for U2 is the pty
row: 100 % ≤ 500 ms, 0 withheld, p50 ≤ pty + 3 ms.

### Harness notes (for U1–U4)

- Probe client: scratchpad `u0/probe.py` (stdlib; frames per
  `docs/core-protocol.md`); the scratch flavors' config dirs
  (`~/.config/flylatt`, `~/.config/flylatp`) are left in place for U2.
- The release's feed port (4939) is taken, so a scratch/dev core logs
  `feed server disabled: bind failed` — harmless here, but U4's `fly-el`
  config should set `feed.port` (or disable the feed) if feed parity is to
  be checked against it.
- **Never `read()` a FIFO fed by a splicing consumer from a process you
  can't afford to lose**: the wait is uninterruptible; `timeout`/SIGTERM do
  nothing. Always pair such a probe with a watchdog that injects more pane
  output or kills the consumer.
- A watcher polling `ls -t /tmp/tmux-1000 | grep <prefix> | head -1` must
  clear stale scratch sockets first or it watches the previous run's
  corpse.
