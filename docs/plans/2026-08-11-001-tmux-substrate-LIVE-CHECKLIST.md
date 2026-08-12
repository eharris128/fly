# tmux substrate — live validation checklist (morning run)

Everything below runs on the **dev flavor** so the installed release stays
untouched. U1–U8 + U10 are implemented behind the flag and live-validated at
the module level (`cargo test --test substrate_live -- --ignored`); this
checklist is the app-level pass that gates the default flip and U9.

## Setup

```bash
# 1. Flip the dev flavor's config (NOT the release one):
#    ~/.config/fly-dev/config.json  →  add: "substrate": "tmux"
# 2. Run the dev build alongside the installed release:
pnpm flavor:dev
```

The mirror change (`mirrorUnfocused`, default on) is substrate-independent —
the release build picks it up on its next normal upgrade; the dev run
exercises both together.

## Checks (R-mapped)

1. **Typing feel (the whole point).** Open 3–4 panes, start `claude` in two,
   get them streaming, then type into the focused pane. Compare against the
   release build side by side. Expect: no hitching — unfocused panes are
   2 Hz snapshots, only the focused pane renders live.
2. **R2 measurement** (protocol from
   `docs/notes/2026-08-11-webkitgtk-engine-floor.md`): with ~5 panes
   streaming, sample the dev webview main thread —
   ```bash
   ps -eo pid,ppid,comm | grep WebKitWebProc   # pick the dev instance's
   python3 /tmp/…/measure.py <pid> <pid> 10    # or re-create: 100ms utime+stime sampling
   ```
   Target: < 20% avg, no sustained >50%.
3. **Mirrors look right.** Unfocused panes show colored, current content;
   clicking one focuses it and reveals the live terminal seamlessly; the
   spinner in a mirror updates ~2×/s.
4. **`leader t` native attach (R1).** Focused claude pane → `leader t` →
   your terminal opens attached to the session. Typing there is native.
   The fly pane shows the "attached in terminal" badge; a raise while
   attached does NOT notify (R9); close the terminal → badge clears.
5. **Restart adoption (R4).** With agents mid-work: quit fly (ordinary
   quit). `tmux -L fly-dev ls` → sessions still there, agents still
   running. Relaunch `pnpm flavor:dev` → panes reattach with scrollback,
   same agents, zero respawns. `leader t` + hooks still work after the
   restart (cross-instance token continuity).
6. **Attention from tmux panes (R3).** A claude in a tmux pane hits a
   permission ask → ring + notification exactly as before.
7. **Feed parity (R5).** With the feed consumer (or curl): roster, output,
   input route, drop against a tmux-backed agent — behavior identical.
8. **Exit surface.** `exit` a shell pane → exited state within ~1.5 s
   (poll floor) or instantly (hook); the final screen stays visible.
9. **Ephemeral hygiene (U10).** Run a `--paned` automation; after its tab
   closes (or a quit), `tmux -L fly-dev ls` shows no leftover automation
   session; `substrate-sessions.json` holds no stale record.

## If something's off

- `substrate: "pty"` (or deleting the key) reverts the substrate entirely.
- `mirrorUnfocused: false` reverts mirrors to live rendering.
- Sessions can always be inspected/killed directly:
  `tmux -L fly-dev ls` / `tmux -L fly-dev kill-server`.

## After a clean pass

- Flip the default (`SubstrateKind::default` → `Tmux`) + release soak.
- Then U9: remove the pty path, the flag, scrollback files, tail ring/vte
  legs (plan KTD9/KTD10), and build the frontend kill-all quit variant +
  settings toggles.
