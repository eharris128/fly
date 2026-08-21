# tmux substrate — live validation checklist

status: **open — 0 of 9 checks recorded.** This checklist gates the
`config.substrate` default flip (`SubstrateKind::default` → `Tmux`) and the
U9 retirements. Record a pass by ticking the box and adding a dated one-line
result under the check; abstain-honest — an unexercised check stays unticked.

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
> Commands) with `"substrate": "tmux"` in `~/.config/fly-el/config.json`;
> `pnpm flavor:dev` remains the Tauri-shell variant. Substitute the flavor
> name in the `tmux -L …` commands below accordingly.

Everything below runs on a **dev flavor** so the installed release stays
untouched. U1–U8 + U10 are implemented behind the flag and live-validated at
the module level (`cargo test --test substrate_live -- --ignored`); this
checklist is the app-level pass that gates the default flip and U9.

## Setup

```bash
# 1. Flip the dev flavor's config (NOT the release one):
#    ~/.config/fly-el/config.json  (or fly-dev for the Tauri shell)
#    →  add: "substrate": "tmux"
# 2. Run the dev build alongside the installed release:
#    Electron: the fly-el dev loop (CLAUDE.md → Commands)
#    Tauri:    pnpm flavor:dev
```

## Checks (R-mapped)

- [ ] 1. **Typing feel (the whole point).** Open 3–4 panes, start `claude` in
      two, get them streaming, then type into the focused pane. Compare
      against the release build side by side. Expect: no hitching.
- [ ] 2. **R2 measurement** (Electron era: sample the Chromium renderer
      process; protocol lineage in
      `docs/notes/2026-08-11-webkitgtk-engine-floor.md`): with ~5 panes
      streaming, sample the renderer main thread.
      Target: < 20% avg, no sustained >50%.
- [ ] 3. **Mirrors look right** (only with `mirrorUnfocused: true` — default
      off since the cutover). Unfocused panes show colored, current content;
      clicking one focuses it and reveals the live terminal seamlessly; the
      spinner in a mirror updates ~2×/s.
- [ ] 4. **`leader t` native attach (R1).** Focused claude pane → `leader t`
      → your terminal opens attached to the session. Typing there is native.
      The fly pane shows the "attached in terminal" badge; a raise while
      attached does NOT notify (R9); close the terminal → badge clears.
- [ ] 5. **Restart adoption (R4).** With agents mid-work: quit fly (ordinary
      quit). `tmux -L <flavor> ls` → sessions still there, agents still
      running. Relaunch → panes reattach with scrollback, same agents, zero
      respawns. `leader t` + hooks still work after the restart
      (cross-instance token continuity).
      *(Supporting evidence: the cutover itself ran this detach→adopt on
      fly-el, 2026-08-12 — migration plan U6. Not yet run as this check.)*
- [ ] 6. **Attention from tmux panes (R3).** A claude in a tmux pane hits a
      permission ask → ring + notification exactly as before.
- [ ] 7. **Feed parity (R5).** With the feed consumer (or curl): roster,
      output, input route, drop against a tmux-backed agent — behavior
      identical.
- [ ] 8. **Exit surface.** `exit` a shell pane → exited state within ~1.5 s
      (poll floor) or instantly (hook); the final screen stays visible.
- [ ] 9. **Ephemeral hygiene (U10).** Run a `--paned` automation; after its
      tab closes (or a quit), `tmux -L <flavor> ls` shows no leftover
      automation session; `substrate-sessions.json` holds no stale record.

## If something's off

- `substrate: "pty"` (or deleting the key) reverts the substrate entirely.
- `mirrorUnfocused: false` reverts mirrors to live rendering (already the
  default since the cutover).
- Sessions can always be inspected/killed directly:
  `tmux -L <flavor> ls` / `tmux -L <flavor> kill-server`.

## After a clean pass

- Flip the default (`SubstrateKind::default` → `Tmux`) + release soak.
- Then U9: remove the pty path, the flag, scrollback files, tail ring/vte
  legs (plan KTD9/KTD10), and build the frontend kill-all quit variant +
  settings toggles.
