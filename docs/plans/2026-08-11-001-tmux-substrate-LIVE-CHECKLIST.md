# tmux substrate — live validation checklist

status: **closed — full pass, 2026-09-01** (spike 2026-08-28-001 U4, run on
`fly-el` with the fixed `fly substrate-pipe` consumer at `a423daa`): checks
1–2 and 4–9 all pass (3 is N/A), plus the proposed check 10 (clean-quit
variant). Evan signed off 1 and 4 at the keyboard; the rest ran agent-driven
over CDP + the feed + borrowed pane tokens. Residuals recorded at the bottom.
This checklist gated the
`config.substrate` default flip (`SubstrateKind::default` → `Tmux`) and the
U9 retirements — **the flip conversation is now open on data; the decision
stays with Evan** (spike results note, gate 3).

> **2026-08-21 reconciliation (post-Electron-cutover):** this checklist was
> written for the Tauri/WebKitGTK shell; the shipped shell is now Electron
> (`docs/plans/2026-08-12-002-…`). Two consequences:
>
> - **Check 2's WebKitWebProc measurement is superseded.** The engine-floor
>   problem it measured is gone on Chromium (5-pane flood 13.8 % renderer
>   main-thread, migration U6), and `mirrorUnfocused` now defaults **off**
>   (U8 measured it a no-op on Chromium). On the Electron shell, check 2
>   means: sample the Chromium renderer process instead; same targets.
> - **Parts of the substrate were exercised in anger during the cutover**
>   (migration plan U6, on `fly-el` with `substrate: "tmux"`): detach on
>   quit → adopt on relaunch was live-verified as the cutover mechanism
>   itself, and the attention/feed/chords parity checklist ran against
>   tmux-backed panes. Those observations support checks 4–7 but were not
>   run as *this* checklist on the dev flavor — they are recorded in that
>   plan, not ticked here.
>
> Setup on the Electron shell: use the `fly-el` dev loop (CLAUDE.md →
> Commands, `pnpm shell:dev`) with `"substrate": "tmux"` in
> `~/.config/fly-el/config.json`. Substitute the flavor name in the
> `tmux -L …` commands below accordingly. (The Tauri-shell variant of this
> setup went with the Tauri shell, 2026-08-27.)

Everything below runs on a **dev flavor** so the installed release stays
untouched. U1–U8 + U10 are implemented behind the flag and live-validated at
the module level (`cargo test --test substrate_live -- --ignored`); this
checklist is the app-level pass that gates the default flip and U9.

## Setup

```bash
# 1. Flip the dev flavor's config (NOT the release one):
#    ~/.config/fly-el/config.json
#    →  add: "substrate": "tmux"
# 2. Run the dev build alongside the installed release:
#    the fly-el dev loop (CLAUDE.md → Commands: pnpm dev + pnpm shell:dev)
```

## Checks (R-mapped)

- [x] 1. **Typing feel (the whole point).** Open 3–4 panes, start `claude` in
      two, get them streaming, then type into the focused pane. Compare
      against the release build side by side. Expect: no hitching.
      *2026-09-01 — Evan, fly-el (tmux) vs installed release (pty) side by
      side, two claudes streaming: "the responsiveness feels fine." Objective
      backing: U2 p50 4.4 ms = pty; in-app REPL probe under check 2.*
- [x] 2. **R2 measurement** (Electron era: sample the Chromium renderer
      process; protocol lineage in
      `docs/notes/2026-08-11-webkitgtk-engine-floor.md`): with ~5 panes
      streaming, sample the renderer main thread.
      Target: < 20% avg, no sustained >50%.
      *2026-09-01 — 5 panes × 33 Hz spinners, renderer main thread sampled
      10 s @ 100 ms: **avg 5.0 %, max 20 %, 0 samples > 50 %** (pty reference
      2026-08-12: 13.8 %). Rider (spike KTD2, the claude-REPL condition):
      keystroke→output-frame measured in-renderer over the real bridge path,
      visible claude composer: **p50 23.3 / p90 24.3 / max 26.3 ms** (n=25,
      25/25 echoed) — no tmux wall for REPL typing.*
- [x] 3. ~~**Mirrors look right**~~ — N/A: the `mirrorUnfocused` mechanism
      was removed with the Tauri shell (2026-08-27-001 KTD8; the migration's
      U8 measured it a no-op on Chromium).
- [x] 4. **`leader t` native attach (R1).** Focused claude pane → `leader t`
      → your terminal opens attached to the session. Typing there is native.
      The fly pane shows the "attached in terminal" badge; a raise while
      attached does NOT notify (R9); close the terminal → badge clears.
      *2026-09-01 — Evan: attach + native typing work; the session renders at
      fly's ~80-col grid, letterboxed ("feels super weird because the ui is
      ugly but i do not really care") — the known U3.2 limitation (the KTD2
      attached-client-wins flip was never wired), not a fail. The R9 cycle
      verified mechanically: raise while a client is attached → ring, **no**
      banner (dbus-watched); `detach-client` → badge gone from the DOM; next
      raise on the same pane banners again.*
- [x] 5. **Restart adoption (R4).** With agents mid-work: quit fly (ordinary
      quit). `tmux -L <flavor> ls` → sessions still there, agents still
      running. Relaunch → panes reattach with scrollback, same agents, zero
      respawns. `leader t` + hooks still work after the restart
      (cross-instance token continuity).
      *2026-09-01 — quit (window-close flow) with 3 claudes + 1 bash live,
      one claude mid-reply: every session + child pid survived the detach;
      relaunch adopted — same paneIds (1/2/4), same pids, the mid-quit reply
      served over the feed post-adopt, zero respawns, no `pane://exit`. A
      pre-restart pane token was accepted by the new core (`fly notify`
      rc=0) and a post-adopt raise rings end-to-end (event `raised`, sidebar
      dots, bell — the deliberate fresh-vs-adopted A/B). Two further
      detach→adopt cycles ran incidentally during checks 10 and the A/B.*
- [x] 6. **Attention from tmux panes (R3).** A claude in a tmux pane hits a
      permission ask → ring + notification exactly as before.
      *2026-09-01 — `fly notify permission --claude` from a pane's borrowed
      env: OS banner over dbus ("Claude needs permission") + ring/history in
      the frontend. Policy legs behave per `state/policy.rs`: a foregrounded
      window suppresses the banner; a visible+foregrounded pane's raise
      instant-acks born-cleared (initially misread as a bug — it isn't).*
- [x] 7. **Feed parity (R5).** With the feed consumer (or curl): roster,
      output, input route, drop against a tmux-backed agent — behavior
      identical.
      *2026-09-01 — feed on :4940 (moved off the release's 4939): SSE roster
      (3 agent rows, bash pane correctly excluded, same wire shape), GET
      output serves the captured reply, POST input submit → the agent
      replied "OK" and the turns tail served it back. Identical to pty
      behavior. (Drop route not re-run — unchanged by the substrate.)*
- [x] 8. **Exit surface.** `exit` a shell pane → exited state within ~1.5 s
      (poll floor) or instantly (hook); the final screen stays visible.
      *2026-09-01 — `exit` → `pane://exit` in the renderer in **17.6 ms**
      (incl. the probe's send-keys subprocess; the U3 wake-pipe hook path,
      not the poll). The dead pane's final screen stays (remain-on-exit;
      capture-pane still serves it).*
- [x] 9. **Ephemeral hygiene (U10).** Run a `--paned` automation; after its
      tab closes (or a quit), `tmux -L <flavor> ls` shows no leftover
      automation session; `substrate-sessions.json` holds no stale record.
      *2026-09-01 — `--paned` one-shot: the ephemeral agent pane ran in the
      Automations workspace, run succeeded, tab auto-closed after the ~6 s
      linger; its tmux session and store record both gone. (The provisioned
      Automations workspace's own durable pane rightly persists.) Closing a
      5-pane tab likewise killed all five sessions and pruned the store.*
- [x] 10. **Cold boot after `kill-server` (R10; added by the 2026-08-28-001
      runbook).** With sessions in the store and the server dead: relaunch →
      sane recovery, not a hang, not a phantom adopt.
      *2026-09-01 — clean-quit variant: relaunch spawned a fresh server, all
      five leaves respawned as fresh shells (dead claudes correctly not
      resurrected), `substrate-server.json` byte-identical (KTD12 token
      reused). The crash-auto-offer leg (clean-exit marker absent) was NOT
      exercised — only the clean-quit path is ticked here.*

## If something's off

- `substrate: "pty"` (or deleting the key) reverts the substrate entirely.
- `mirrorUnfocused: false` reverts mirrors to live rendering (already the
  default since the cutover).
- Sessions can always be inspected/killed directly:
  `tmux -L <flavor> ls` / `tmux -L <flavor> kill-server`.

## Residuals from the 2026-09-01 run (recorded, non-blocking)

- **fd leak into the detached tmux server.** The shell's listening sockets
  are inherited down the spawn chain (shell → `fly core` → tmux server) and
  outlive the app: after quitting a dev shell launched with
  `--remote-debugging-port=9222`, the *tmux server* held the port's LISTEN
  socket (fd 64), blocking the next launch's devtools bind. Dev-only
  trigger, but the mechanism is general — the long-lived server keeps any
  non-CLOEXEC fd the shell had open. Fix shape: close-fds/CLOEXEC hygiene on
  the core-spawn and server-spawn seams.
- **One-off, unreproduced:** in one adopted instance (the second of the
  day), three raises reached the renderer's history (recorded unread) while
  the feed roster's `needsAttention` stayed false across three reads. A
  deliberate fresh-vs-adopted A/B afterwards behaved correctly on both legs
  (event `raised`, dots, bell), and dashboard-open was separately exonerated.
  Watch for it; evidence in the spike results note §U4.
- **Check 10's crash leg**: the crash auto-offer (clean-exit marker absent)
  remains unexercised on the substrate.
- **Check 4 geometry**: attach shows fly's grid letterboxed — the never-wired
  KTD2 flip (spike note §U3.2), already on the substrate plan's residuals.

## After a clean pass

- Flip the default (`SubstrateKind::default` → `Tmux`) + release soak.
- Then U9: remove the pty path, the flag, scrollback files, tail ring/vte
  legs (plan KTD9/KTD10), and build the frontend kill-all quit variant +
  settings toggles.
