# Spike: tmux 3.6 substrate validation — 2026-08-28-001

**Status**: **U0 executed 2026-08-28** — results in
`docs/notes/2026-08-28-tmux-36-substrate-spike.md`. Baseline recorded (pty
4.3 ms / 100 %; tmux p50 ≈ inter-key gap, last write withheld); the three
`substrate_live` exit-family failures on 3.6 are **the same consumer bug**,
not a tmux delta — the FIFO reader's `close()` (and `read()`) block
uninterruptibly behind uutils `cat`'s `splice()`, which sleeps on the socket
while holding the destination pipe's mutex. tmux 3.6 exonerated; Q2 collapses
to a confirmation pass after U1. U1 next.
**Type**: spike (bounded validation + one targeted fix; not a feature)
**Follows**: `docs/plans/2026-08-11-001-feat-tmux-session-substrate-plan.md`
(the substrate, built and module-validated on tmux 3.4),
`docs/plans/2026-08-11-001-tmux-substrate-LIVE-CHECKLIST.md` (0 of 8 live
checks recorded), the 2026-08-23 measurement that sent the release back to
`pty` (tmux path echoes ~1 keystroke in 3), and the 2026-08-28 reproduction
below.
**Feeds**: the LIVE-CHECKLIST (this spike *records* its checks), the
`config.substrate` default-flip decision, and the README's wording for the
open-source release (today it advertises the substrate as a working opt-in).

## What we know going in (2026-08-28)

The withholding is **not** in fly, tmux's reader, the coalescer, or the
control-mode client. It is the `pipe-pane` consumer:
`substrate/tmux.rs::pipe_pane_open` arms `pipe-pane -o "cat > '<fifo>'"`, and
`/usr/bin/cat` on this box is **uutils coreutils 0.8.0** (Ubuntu's default
since 25.10; package `coreutils-from-uutils`), which copies with
`splice(socket → FIFO)`. On that kernel path single-byte echoes arrive in
clumps of 2–3 and the last byte is held until the pipe closes. Reproduced on
a scratch `tmux -L` server with a reader shaped exactly like fly's
(`O_RDWR|O_NONBLOCK` + `poll`), 12 one-byte echoes:

| consumer | reads | timing |
|---|---|---|
| uutils `cat` (shipped) | 5 | `3@255 3@616 2@863 3@1228 1@2708 ms` — last byte on pipe close |
| busybox `cat` (sendfile/splice) | same clumping | |
| GNU `cat` (`/usr/bin/gnucat` 9.7) | 12 | each ≤ 6 ms |
| `dd bs=65536 status=none of=…` | 12 | each ≤ 6 ms, at 30 ms key gaps |
| python `os.read`/`os.write` loop | 12 | each ≤ 6 ms |

Also pinned: FIFO-specific (uutils `cat` → regular file is fine); an attached
control-mode client changes nothing; under `strace` the same `cat` delivers
promptly (a heisenbug for the obvious diagnostic). GNU-cat immunity is why
the substrate measured clean at build time and why the failure read as
"input lag" here. Memory: `tmux-substrate-withholds-output`.

## Questions (dependency order)

- **Q1 — Does a fly-owned consumer restore pty parity through the product
  path?** Not on a rig: through `fly core` over its control socket, the
  2026-08-23 probe repeated on the fix.
- **Q2 — Does the substrate as built on 3.4 behave on tmux 3.6a** (this box:
  `3.6a-2ubuntu0.1`)? The module live tests have never run on it. The 3.6
  changelog touches none of the primitives fly uses (`pipe-pane -o`,
  `send-keys -H`, `set-hook -t <session> pane-died|client-attached|
  client-detached`, `remain-on-exit`, `window-size manual|latest` +
  `resize-window`, `exit-empty off`, `history-limit`, `-e` env at
  `new-session`, `-C` control mode) — it adds a `command-error` hook and
  `remain-on-exit-format`, and reverts `run-shell` to `/bin/sh` (good: our
  hook commands no longer depend on `default-shell`). So Q2 is *confirm*, not
  *expect to fix* — but confirm on this version, with `tmux -V` on every
  result line. **U0 answered most of it**: the suite's 3 failures on 3.6a
  traced to the consumer bug (results note §U0.1–U0.2), and every primitive
  checked by hand on a scratch 3.6 server behaves as on 3.4.
- **Q3 — Does the app-level checklist pass on `fly-el`** far enough to (a)
  keep the README's "opt-in" sentence honest for the release and (b) open the
  default-flip discussion with data instead of a feeling?

## Non-goals

- No default flip, no U9 retirements, no new substrate features (multi-client
  attach policy, control-mode streaming, per-provider submit tables), no
  Wayland leg. Those are the substrate plan's deferred list and stay there.
- The installed release is never touched: scratch `tmux -L flyspike-*`
  servers, an isolated `FLY_APP_NAME=flylat` core, and the `fly-el` dev loop
  only. The release stays on `pty` throughout.
- Not a latency spike. The engine/pipeline numbers exist (41.8 ms packaged
  echo p50, migration U6). The only numbers this spike produces are echo
  **completeness** and the transport hop on the fixed consumer.

## Key technical decisions

- **KTD1 — fly owns the consumer.** `pipe-pane -o "'<fly_bin>' substrate-pipe
  '<fifo>'"`: a new CLI op beside `substrate-event` (`cli/substrate.rs`;
  "launched by a display shell, not typed by humans"), reading stdin in
  64 KiB chunks and writing each read to the FIFO as-is; exits 0 on stdin
  EOF or `EPIPE`, never logs, ignores nothing else. `fly_bin` is already
  threaded to the spawn path for hooks (`Substrate::fly_bin`); the command
  string is built under the same quote-rejection rule the hooks use
  (`hooks_reject_quoted_paths_and_embed_validated_name`). **Must be a plain
  `read`/`write` loop — not `std::io::copy`**, whose Linux specialization
  tries `copy_file_range` → `sendfile` → `splice` for fd pairs and would
  reintroduce exactly this bug (uutils `cat` is Rust; that is very likely how
  it got here). `is_cli_subcommand` gains the name, or a bare
  `fly substrate-pipe` would exec the Electron shell (2026-08-27-001 KTD7).
  Rejected alternatives: `dd bs=65536 status=none` works (proven above) and
  is the one-line fallback if U1 slips, but `status=none` support across
  busybox builds is a guess and any host binary is a repeat of the same class
  of bug; GNU `cat` is not something fly can require.
- **KTD2 — the acceptance test runs through the product.** The 2026-08-23
  probe, unchanged: an isolated `FLY_APP_NAME=flylat` `fly core`, a pane
  spawned over the control socket (`docs/core-protocol.md`; `control/
  registry.rs`) running `cat` (line-discipline echo = exactly one byte per
  key), keystrokes down the 0x03 frame, output up the 0x02 frame, ≥ 50 keys
  per run at 10/30/100/300 ms gaps, then a 2 s idle. Pass = 100 % of keys
  echoed *and* zero bytes released by the idle; hop p50 within +3 ms of the
  same probe on `pty` (4.6 ms, which is the 4 ms `VISIBLE_FLUSH` + PTY). A
  second condition with a real `claude` REPL (many-byte redraws, the realistic
  per-key cost) records keystroke→frame p50 vs `pty`.
- **KTD3 — pin the bug as a live test, not a note.** `tests/substrate_live.rs`
  gains `pipe_consumer_delivers_every_byte` (`#[ignore]`, scratch server like
  its siblings): 12 single-byte echoes at 30 ms → 12 distinct reads, each
  within 50 ms of its send, none released at pipe close. It fails on today's
  `cat` consumer on this box and passes on KTD1. A future contributor who
  "simplifies" the consumer back to `cat` (or to `io::copy`) gets told by the
  test, not by a user on uutils.
- **KTD4 — scratch first, product second.** Every mechanism question is
  answered on a throwaway `tmux -L flyspike-*` server (sub-minute iterations,
  zero fly state — the shape of the consumer matrix) before it is confirmed
  through `fly core` (KTD2) and then the app (U4). Never the other way round.
- **KTD5 — checklist discipline unchanged.** Abstain-honest, dated one-line
  results, `tmux -V` on each. Checks driven by the agent say so ("driven over
  control socket / CDP"); the two that need a human at the keyboard (1 and 4)
  say who did them. Nothing is ticked from supporting evidence alone — the
  2026-08-12 cutover observations stay where they are.
- **KTD6 — two harness traps are pre-empted.** (i) Don't diagnose the consumer
  under `strace` — the timing perturbation hides the bug; use reader-side
  timestamp logs. (ii) In the Claude Code Bash sandbox, `sleep` is killed
  (use `python3 -c 'time.sleep(…)'`) and a *blocking* FIFO reader wedges in
  D-state — open `O_RDWR|O_NONBLOCK` and `poll`, i.e. fly's own shape.

## Units

- **U0 — baseline on 3.6a, no code change** (~30 min). **Done 2026-08-28**
  — see the results note; took a half day because the 3/6 test failures
  had to be run to ground (they were worth it: one mechanism now explains
  the lag, the withheld last write, and the failures). `cargo test --offline
  --manifest-path core/Cargo.toml --test substrate_live -- --ignored` (first
  ever run on 3.6a; answers most of Q2 before anything is touched), then the
  KTD2 probe against current `main` to record the *before* numbers on this
  tmux + kernel (expected: the ~1-in-3 failure). Record both in the results
  note.
- **U1 — the consumer fix + its test** (KTD1, KTD3). `cli/substrate.rs`
  (or a sibling `cli/pipe.rs`): `fly substrate-pipe <fifo>`; `Tmux::
  pipe_pane_open(name, fifo, fly_bin)` with an arg-construction unit test
  beside `hooks_reject_quoted_paths…`; `pty/pane.rs::spawn_tmux` passes
  `substrate.fly_bin()`; `is_cli_subcommand` + the help text; the
  `pipe_consumer_delivers_every_byte` live test, run red on U0's tree and
  green on this one. CLAUDE.md's substrate paragraph and the substrate plan's
  build log gain the one-paragraph scar ("never a host `cat`; never
  `io::copy`").
- **U2 — acceptance probe on the fix** (KTD2). Both conditions (`cat` pane;
  `claude` REPL), `pty` run alongside for the parity column. This is the gate
  for everything after it.
- **U3 — 3.6 conformance.** Whatever U0 left unanswered, explicitly: after a
  detach→adopt (`restart_roundtrip_…`), do `pane-died` and the attach hooks
  still reach the socket on the *new* instance (KTD12 token continuity), and
  what is the exit-surface latency on the hook path vs the 1.5 s poll floor
  (record both, ≥ 10 trials); does a detached session keep `window-size
  manual` geometry across an attach/detach from a real terminal; does
  `history-limit` at server spawn still bind panes as KTD9 assumes. A failure
  here is fixed inside the spike only if it is one unit's worth; otherwise
  recorded and the spike stops at the gate.
- **U4 — the checklist on `fly-el`.** `~/.config/fly-el/config.json` →
  `"substrate": "tmux"`; `pnpm dev` + `pnpm shell:dev`; `tmux -L fly-el ls`
  first and kill any stale marked sessions from an earlier dev session (they
  would be adopted). Checks **2, 5, 6, 7, 8, 9** driven by the agent with the
  existing toolkit (CDP `--remote-debugging-port`, the control socket, the
  borrowed-token `fly notify` technique, `dbus-monitor` for banners); checks
  **1 and 4** by Evan from a prepared script: side-by-side fly-el-on-tmux vs
  the installed release-on-pty, two `claude` panes streaming, type into the
  focused pane; then `leader t`, native typing in the terminal, a permission
  ask while attached → no banner, badge clears on close. Each result lands in
  the LIVE-CHECKLIST as a dated line (KTD5). Propose and, if it passes, add
  **check 10 — cold boot after `kill-server`** (R10: store records + no
  server ⇒ the resume offer, not a hang or a phantom adopt), since
  `substrate-server.json` deliberately survives a server kill.
- **U5 — results note + decisions.** `docs/notes/2026-08-28-tmux-36-substrate-
  spike.md`: the U0/U2 tables, the U3 findings, the checklist status; update
  the LIVE-CHECKLIST header, the substrate plan's status line, CLAUDE.md,
  and the README sentence per the gate below; refresh memory
  (`tmux-substrate-withholds-output` → resolved, `electron-cutover-pending`'s
  open-items list). The default-flip *recommendation* goes in the note; the
  decision is Evan's and is not part of this spike.

## Decision gate (set before any measurement)

1. **U2 completeness**: 100 % of ≥ 50 keys echoed at every gap, zero bytes
   released by the 2 s idle, hop p50 ≤ `pty` + 3 ms. **Miss ⇒ stop**: the
   consumer was not the whole story; write up U0–U2 and do not start U4.
2. **U3 conformance**: all `substrate_live` tests green on 3.6a; hook-path exit
   surfacing < 300 ms in ≥ 8 of 10 trials (the poll floor is the fallback,
   not the norm); geometry and history behave as the plan's KTD2/KTD9 assume.
3. **U4 checklist**: checks **1, 4, 5, 6, 8** pass — the five the README
   sentence actually promises (agents outlive the app, restart adopts, native
   typing via `leader t`, attention from tmux panes, honest exit surface).
   Checks 2, 7, 9 are recorded either way.

Outcomes: all three hit → the README keeps "opt-in" as written and the
default-flip conversation opens on data. Gate 1 or 2 missed → the README
says "experimental — known issue: …" for the open-source release and the
substrate stays behind the flag with the note as the handoff. Gate 3 partial
→ the checklist records exactly what passed; README says "experimental".

## Harness (reused, not rebuilt)

- Consumer matrix: `reader.py` (O_RDWR|O_NONBLOCK + poll FIFO reader with
  per-read timestamps) + `matrix.sh` (scratch server, pane running `cat`,
  `pipe-pane` per consumer, 12 keys at a chosen gap, size/timing report) —
  session scratchpad today; U1 turns them into the live test so they are
  preserved in the tree, not in a scratchpad.
- KTD2 probe: the 2026-08-23 control-socket frame client (isolated `flylat`
  core; `docs/core-protocol.md` frames 0x02/0x03) — re-derived from the
  protocol doc if the scratchpad copy is gone; ~60 lines of Python.
- App driving: CDP driver + `/proc` renderer sampler + XTEST fallback and the
  borrowed-token socket technique (memory
  `dev-flavor-live-validation-techniques`, `electron-cutover-pending`).

## Risks / notes

- **The fix is one line away from being wrong again.** `io::copy`, `cat`,
  busybox, `splice` — all natural, all broken on this path. KTD3's test is
  the only durable defence; land it with U1, not after.
- A leftover `tmux -L fly-el` server from an earlier dev session is
  *adopted*, not ignored — inspect before U4.
- `FLY_APP_NAME` must be set on every packaged-shell or core launch in this
  spike; the packaged shell defaults to flavor `fly` and would adopt the
  installed release's core.
- Checks 1 and 4 need Evan for ~20 minutes at a keyboard; everything else is
  unattended.
- Budget: U0–U3 one session (half a day including the numbers); U4 a second
  session around the human checks; U5 an hour. Timebox: if U3 finds a 3.6
  behaviour that isn't one unit's worth, record it and stop at gate 2 rather
  than growing the spike.
