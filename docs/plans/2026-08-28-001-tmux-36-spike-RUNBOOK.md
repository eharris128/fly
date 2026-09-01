# tmux 3.6 substrate spike — runbook for the remaining units (U3 → U5)

Companion to `docs/plans/2026-08-28-001-spike-tmux-36-substrate-validation-plan.md`
(the decisions) and `docs/notes/2026-08-28-tmux-36-substrate-spike.md` (the
numbers). This file is the **operational script**: what runs unattended,
what needs Evan at the keyboard, the exact commands, what "pass" looks like,
and where each result gets written. Written 2026-08-28 so the next session
can start at the co-working step without re-deriving anything.

## Where we are

| unit | state | evidence |
|---|---|---|
| U0 baseline | done | note §U0 — pty 4.3 ms / 100 %; tmux p50 ≈ key gap; 3/6 live tests failing, root-caused |
| U1 consumer fix | done, committed `0c52a21` | `fly substrate-pipe`; live suite 7/7 (was 3/6) |
| U2 acceptance | done, **gate 1 hit** | tmux 50/50 at every gap, p50 4.34–4.47 ms vs pty 4.23–4.29, zero withheld |
| U3 3.6 conformance | done, **gate 2 hit** | note §U3 — hook-path exit 501 ms → **10 ms median** (wake pipe); history-limit ok; attach-geometry flip never wired (known limitation) |
| U4 checklist on fly-el | done 2026-09-01, **gate 3 hit — full pass** | LIVE-CHECKLIST ticked; note §U4 |
| U5 results + wording | done 2026-09-01 | note §U5; README unchanged ("opt-in" stays); CLAUDE.md caveat resolved |

Safety rule for every step, restated: nothing here touches the installed
release (flavor `fly`, `/run/user/1000/fly/*`, `tmux -L fly`, the core that
hosts the pane this session runs in). Everything runs on `flylatt`/`flylatp`
(scratch cores) or `fly-el` (the dev flavor, isolated by `FLY_APP_NAME`).

## U3 — 3.6 conformance (agent, unattended, ~30 min) — DONE 2026-08-28

Gate 2 of the plan — hit. Kept below as the record of what was run; results
in the note §U3. Two things U4 inherits: **check 8** should now see an exit
surface in ~10 ms (not "~1.5 s or instantly"), and **check 4** will show the
attached terminal at fly's 80-column-ish grid rather than filling the
terminal — the KTD2 flip was never built; judge the typing, not the size.

**U3.1 Hook-path exit latency.** Question: when a pane's child dies, how
long until `pane://exit` reaches a control client *with no `panes_status`
polling* — the pure `pane-died` hook → `fly substrate-event` → hook socket →
`force_dead` → reader path. Method: `flylatt` core (`substrate: "tmux"`),
10 trials of `spawn_pane` running `sh -c 'read x; exit 7'`, then a `\n`
down the 0x03 frame (the death instant is the send), stopwatch until the
matching `pane://exit` event. Script: scratchpad `u3/u3_exit.py`. Target
from the plan: < 300 ms in ≥ 8 of 10. **Known structural ceiling:** the
reader wakes from a 500 ms `poll()` timeout — `force_dead` only sets a flag
and pokes the pause condvar, it does not wake the FIFO poll — so latencies
should scatter uniformly 0–500 ms and the target may fail *by design*. If
it does, that is a finding, not a 3.6 regression, and the one-unit fix is
to put a self-pipe/eventfd in the reader's poll set (out of scope for the
spike; record it for the plan's residuals).

**U3.2 Geometry across attach/detach (KTD2).** Spawn a pane at 80×24;
confirm `window-size manual` and `80x24`; attach a real client sized
100×30 (`script -qfc 'stty cols 100 rows 30; exec tmux -L flylatt attach
-t fly-flylatt-u3geo' /dev/null` — the S4 trick, no tty needed); expect
the `client-attached` hook to reach fly and fly to flip the window to
`latest` (dims follow the client: `100x30`); kill the client; expect
`manual` + fly's grid again. Script: `u3/u3_geo.sh`.

**U3.3 history-limit at server spawn (KTD9).** `show-options -gv
history-limit` on the flylatt server = 10000, and a freshly spawned pane's
`#{history_limit}` = 10000. Same script.

Recording: a `## U3` section in the results note; the plan's U3 line marked
done; gate 2 verdict stated. Scratch: `tmux -L flylatt kill-server` at the
end; the `~/.config/flylatt` / `flylatp` dirs stay until U5.

## U4 — the checklist on `fly-el` (the co-working session)

This is `docs/plans/2026-08-11-001-tmux-substrate-LIVE-CHECKLIST.md`, run
for real on the dev flavor, on the fixed consumer. Split: the agent drives
**2, 5, 6, 7, 8, 9** over CDP + the control socket + the hook socket; Evan
drives **1 and 4** (typing feel, native attach) — the two that are a human
judgement. Budget: ~20 min of Evan's keyboard time, ~1 h total.

### Setup (agent, before Evan sits down)

```bash
# 0. Pre-flight facts as of 2026-08-28: no `tmux -L fly-el` server, no
#    ~/.local/share/fly-el/substrate-sessions.json, ~/.config/fly-el/config.json
#    holds only a feed token. Re-check; a leftover dev server would be ADOPTED.
tmux -L fly-el ls 2>&1; ls ~/.local/share/fly-el/ 2>&1

# 1. Flip the DEV flavor (never ~/.config/fly/). feed.port moves off 4939,
#    which the installed release owns — otherwise fly-el's feed silently
#    fails to bind and check 7 has nothing to talk to.
python3 - <<'EOF'
import json, os
p = os.path.expanduser("~/.config/fly-el/config.json")
c = json.load(open(p))
c["substrate"] = "tmux"
c.setdefault("feed", {})["port"] = 4940
json.dump(c, open(p, "w"), indent=2); print(json.dumps(c, indent=2))
EOF

# 2. The dev loop (two terminals, or two background jobs from the agent):
pnpm dev                                   # Vite on :1420
cargo build --manifest-path core/Cargo.toml   # already built at 0c52a21+
pnpm shell:dev                             # Electron, FLY_APP_NAME=fly-el, X11 via Xwayland on :0
#    For agent driving add: ELECTRON_EXTRA_ARGS / edit the script to pass
#    --remote-debugging-port=9222 (CDP). python3-xlib is NOT installed on
#    this box any more, so XTEST is out; CDP `Runtime.evaluate` is the way in.

# 3. Confirm the substrate is live: after the window opens,
tmux -L fly-el ls            # one fly-fly-el-<leaf> session per pane + flyctl-input
```

Revert at any point: set `"substrate": "pty"` (or delete the key) in
`~/.config/fly-el/config.json` and relaunch; `tmux -L fly-el kill-server`
clears the sessions.

### Agent-driven checks (record each as a dated line in the LIVE-CHECKLIST)

- **Check 2 — renderer main thread under ~5 streaming panes.** Open 5 panes
  (CDP → the app's split/new-tab chords or `spawn_pane` via the control
  socket at `$XDG_RUNTIME_DIR/fly-el/control.sock`), start a 33 Hz spinner
  in each (`while :; do printf '\r%s' "$(date +%N)"; sleep 0.03; done`),
  sample the Chromium *renderer* main thread (`/proc/<pid>/task/<tid>/stat`
  at 100 ms for 10 s — the renderer is the child of the sandboxed zygote
  owning a `Compositor` thread). Target: < 20 % avg, no sustained > 50 %.
  Reference: 13.8 % on 2026-08-12 (pty, Electron U6).
- **Check 5 — restart adoption (R4).** With two `claude` panes mid-work:
  quit the shell normally (CDP `window.close()` / the quit chord) →
  `tmux -L fly-el ls` still lists the sessions and `ps` shows the same
  `claude` pids → relaunch `pnpm shell:dev` → panes reattach with
  scrollback, **same pids**, no respawn (`pane://exit` never fires; the
  resume store is untouched). Then `fly notify` with a borrowed pane token
  from `/proc/<claude pid>/environ` must still raise (token continuity).
- **Check 6 — attention from tmux panes (R3).** A `claude` in a tmux pane
  hits a permission ask → ring on the pane/tab + OS banner (`dbus-monitor
  --session "interface='org.freedesktop.Notifications'"`). Cheaper
  equivalent without a real ask: `fly notify permission --claude </dev/null`
  from a shell holding the pane's env (uid-only peer-cred).
- **Check 7 — feed parity (R5).** `curl -H "Authorization: Bearer $(python3
  -c 'import json,os;print(json.load(open(os.path.expanduser("~/.config/fly-el/config.json")))["feed"]["token"])')"
  http://127.0.0.1:4940/agents/<key>/output`, the SSE `/feed`, and a
  `POST …/input` submit into a tmux-backed agent — same shapes as on pty.
- **Check 8 — exit surface.** `exit` in a shell pane → the pane shows exited
  essentially at once (hook path + wake pipe: 10 ms median in U3.1; the
  1.5 s `panes_status` poll remains the lost-hook floor) and the final
  screen stays.
- **Check 9 — ephemeral hygiene (U10).** `fly automation create --paned …`
  a one-shot; after its tab closes (or a quit) `tmux -L fly-el ls` shows no
  automation session and `substrate-sessions.json` no stale record.
- **Proposed check 10 — cold boot after `kill-server` (R10).** With
  sessions in the store: quit fly, `tmux -L fly-el kill-server`, relaunch →
  the resume offer (not a hang, not a phantom adopt); `substrate-server.json`
  (the persisted KTD12 token) is simply reused.

### Evan-driven checks (the co-working bit, ~20 min)

**Check 1 — typing feel, the whole point.** Side by side: fly-el (tmux,
fixed consumer) and the installed release (pty). In fly-el open 3–4 panes,
start `claude` in two and get them streaming (ask for something long), then
type into the focused pane — a shell, then a `claude` composer. Compare with
the same motion in the release window. Expect: no hitching, no
"character appears when I type the next one", the composer keeps up. The
objective half is already recorded (U2: tmux p50 4.4 ms = pty); this is the
subjective sign-off. Record: `[x] 1. 2026-08-xx — Evan: …` one line.

**Check 4 — `leader t` native attach (R1/R9).** Focus a `claude` pane →
`leader t` → your terminal opens attached to that session; type there —
native latency by construction. (Expect the session at fly's grid size,
letterboxed if your terminal is bigger — the attached-client-wins flip was
never wired, see note §U3.2; that is a known limitation, not a fail.) Back in fly, the pane shows the
"attached in terminal" badge. Now make the agent raise attention (ask it
something that needs a permission) **while attached**: expect **no** OS
banner (attached-elsewhere suppresses, R9) though the pane/tab still rings.
Close the terminal → badge clears; the next raise banners again. Record one
line each for the attach, the suppression, and the clear.

If either feels wrong, stop and say so — the objective numbers are in the
note; a subjective miss is exactly the signal the checklist exists to catch.

## U5 — results note + decisions (agent, ~1 h)

1. Results note: `## U3`, `## U4` sections with the checklist lines copied;
   the `claude`-REPL latency condition from KTD2 (a REPL pane exists in U4 —
   measure keystroke→frame with the same probe on `fly-el`'s control socket).
2. LIVE-CHECKLIST: tick what passed with dated lines; update the status
   header; add check 10 if it ran.
3. Gate 3 verdict → README wording (`"opt-in"` stays as written if checks
   1, 4, 5, 6, 8 pass; else `"experimental — known issue: …"`); the spike
   plan's status; the substrate plan's status line; CLAUDE.md's substrate
   paragraph loses the "until live validation" caveat only if everything
   passed.
4. Memory: `tmux-substrate-withholds-output` → closed; `electron-cutover-
   pending` open-items list; a `dev-flavor-live-validation-techniques`
   addendum (CDP over XTEST — python3-xlib is gone; feed.port collision).
5. Scratch cleanup: `rm -rf ~/.config/flylatt ~/.config/flylatp
   ~/.local/share/flylatt ~/.local/share/flylatp`, `tmux -L flylatt
   kill-server`, stale `/tmp/tmux-1000/fly{latt,latp}` sockets. Leave
   `fly-el`'s config on whatever Evan wants for daily dev; note it.
6. Commit; the default-flip *recommendation* goes in the note — the
   decision stays Evan's and is not part of this spike.
