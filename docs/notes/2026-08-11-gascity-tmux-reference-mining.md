# Gas City tmux-runtime mining — 2026-08-11

Bounded reference pass over github.com/gastownhall/gascity (the extracted SDK
of Steve Yegge's Gas Town — a Go multi-agent orchestrator whose default
runtime provider is tmux), read at shallow-clone HEAD 2026-08-11, keyed to the
decision inventory in
`docs/brainstorms/2026-08-11-tmux-session-substrate-requirements.md`.
**Pattern reference, not a dependency** (same verdict as the 2026-07-06
conductor-oss evaluation). Files cited: `internal/runtime/tmux/tmux.go`
(~4.2k lines, heavily scar-commented), `state_cache.go`, `interaction.go`,
`internal/runtime/{runtime,dialog,carrier}.go`, `internal/session/manager.go`.

## D1/D2 — Observation

- **No control mode, no pipe-pane** (pipe-pane appears only in their k8s
  provider, for log shipping). All observation is **subprocess polling**:
  `list-sessions`/`list-panes`/`list-windows -F` behind a **2 s-TTL
  singleflight `StateCache`** with a 30 s "too stale to trust" ceiling
  (after which `IsRunning` returns false *and logs degraded* rather than
  serving ancient truth). One snapshot serves all sessions per tick —
  the same shape as our `panes_status` batching.
- They **never render panes** — humans attach directly; the GUI-mirror
  problem doesn't exist for them. No signal on mirror tiers; strong signal
  that *orchestration-grade* observation needs only cheap polling.
- Output IS parsed, narrowly: `capture-pane -p -S -N` regex scans for
  Claude's busy indicator ("esc to interrupt"), approval prompts
  (`interaction.go` — hash-deduped), and startup dialogs (`dialog.go`).
  Regex-on-capture is *weaker* than our vte-grid + abstain-on-surprise
  (`feed/screen.rs`); nothing to steal there — our parser is ahead.
- **Activity gotcha (steal this):** for detached sessions,
  `#{session_activity}` does NOT advance on pane I/O — it sticks at
  creation/attach. They query per-window `#{window_activity}` and take the
  max. Any fly activity read off tmux formats must do the same.
- **Poke-echo discounting:** their own send-keys advances window activity,
  so a woken-but-dead agent looks perpetually active. They snapshot genuine
  activity before each injected write and discount the echo (3 s echo
  window, 15 s grace). fly's equivalent hazard: peer/drop/handoff injection
  polluting `state/activity.rs`'s working/idle read through a tmux-backed
  activity source.

## D4 — Identity / adoption

- Sessions are **name-marked**: `s-<beadID>` (`session/manager.go::
  sessionNameFor`, `/` → `--`), names validated against
  `^[a-zA-Z0-9_-]+$` — dots and colons make tmux *silently misbehave*
  (they're target-syntax metacharacters). Their store, not tmux, is the
  source of truth; `CleanupOrphanedSessions` takes an injected
  `isGTSession` predicate and touches nothing else.
- **They never adopt unmarked sessions** — supports our prior. The inverse
  arm: a *marked* session whose agent process died (tmux alive, agent dead
  = "zombie") is killed by the sweep, and `EnsureSessionFresh` uses
  **create-first** (attempt `new-session`, classify `ErrSessionExists`)
  to dodge the check-then-create TOCTOU under concurrent creators.
- **Destructive arms defer on observation failure** (`runtime.go::
  ErrRuntimeUnavailable` doc): an unreachable tmux server / failed process
  scan is "I could not tell", never "all sessions are gone". Their
  reconciler's close-as-orphaned/sweep arms explicitly refuse to act on it.
  Our D7 orphan sweep must adopt this rule verbatim.

## D5 — Input

- Delivery ladder: `send-keys -l` (literal) for text ≤ 4096 bytes, falling
  back to `load-buffer` from a temp file + `paste-buffer -p -d` (forced
  bracketed paste) for longer — plus retry-with-backoff on the transient
  `"not in a mode"` error a still-initializing TUI returns.
- **The submit-Enter drop race (ga-bwm — steal the whole lesson):** an
  Enter sent after a paste can be swallowed racing an unfinished bracketed
  paste or a detached-pane wake, leaving text *drafted but never
  submitted*, silently, for minutes. Their fix: **verified submit** —
  after Enter, poll the busy indicator; re-send Enter only while the pane
  is still idle (a busy pane never gets a second Enter, so no
  double-submit); an unconfirmed submit is surfaced as its own error and
  treated as undelivered by retrying callers. fly's `deliver_with_guards`
  paste→re-probe→Enter ordering has no post-Enter confirmation today; a
  tmux-backed delivery path should add this.
- **SIGWINCH wake (critical, and our S3 missed it):** a Claude TUI in a
  *detached* session may not process stdin until a terminal event occurs.
  They wake it with a resize-pane -1/+1 dance (`WakePane`), skipped when a
  client is attached. Our S3 pipe fidelity test used `cat` (not a raw-mode
  TUI), so it couldn't see this. Any detached-delivery path in fly needs
  the wake.
- **Escape hazards, corroborated:** their per-provider table *skips* the
  default pre-Enter Escape for `claude` (and most TUIs) because Escape
  clears input / cancels state — independent confirmation of our
  ESC-cancels-picker analysis from the other direction. Submit sequences
  are per-provider data (`nudgeSubmitKeySequences`), not code — a shape
  worth copying if fly ever drives non-Claude agents.
- Nudges to the same session are serialized by per-session timed locks —
  concurrent sends interleave into garbled input otherwise. fly's
  app-wide delivery mutex already covers this.
- Some providers (gemini, not claude) need a real attached client for
  interrupts; their **hidden attach** runs `tmux attach` under
  `script -qfc` (the same pty trick our S4 used), readiness signaled
  event-first via `set-hook client-attached` + `wait-for -S` with a poll
  fallback.

## D6/D7 — Lifecycle

- Attach state read via `#{session_attached}`; used to gate the SIGWINCH
  wake. No focus/suppression analog (they have no notification pipeline) —
  no signal on D6's policy question, but the primitive is confirmed cheap.
- **Supervisor restarts are a non-event by construction:** session names
  are deterministic from the store, so rediscovery is `IsRunning(name)`
  per stored worker — no handshake, no re-registration. The lesson for our
  reattach: derive everything from (store record, session name), never
  from in-memory state that died with the process.
- **Exit detection is event-driven, not reaped:** `set-hook pane-died`
  runs a command carrying `#{pane_dead_status}` (requires
  `remain-on-exit on`). Their auto-respawn hook chains respawn + re-arm —
  and documents that **`respawn-pane` resets `remain-on-exit` to off**
  (gotcha). For fly this can replace the per-pane reaper thread's
  exit-detection role wholesale: pane-died → `run-shell` → a `fly` CLI
  call over the hook socket.
- **ga-h9z, the nastiest scar in the file (steal verbatim):** `new-session`
  against a *wedged-but-bound* server socket lets tmux's own very short
  liveness probe time out, **unlink the socket, and spawn a parallel
  server — orphaning every session on the original**. Their guard: a
  bounded `has-session` preflight against a deliberately-unrouteable name;
  a healthy server answers "session not found", a dead one "no server
  running", anything else ⇒ `ErrServerDegraded`, refuse to create, tell
  the human. Our D7 startup/creation path needs this probe.
- Server options they force: `exit-empty off` (otherwise the server exits
  with its socket when the last session dies — fly wants the server
  durable), `-u` on every invocation (UTF-8 regardless of locale), and
  per-session `window-size latest` — **tmux 3.3+ pins detached sessions
  at 80×24 (`window-size manual`) otherwise**. That last one directly
  hits our mirrors: fly must drive each session's window size to the fly
  pane's grid (and re-drive it on fly-side resize), or every mirror and
  screen-parse sees an 80×24 fiction.

## D10 — Env / token

- Injection is `new-session -e KEY=VALUE` per session (documented floor:
  **tmux ≥ 3.2**), keys sorted for determinism — validates our S1 approach.
- **The overlay trap (their hardest-won env lesson, falsified against tmux
  3.4):** session env is an *overlay on the server's global env* — a var
  you merely omit arrives anyway, carrying whatever the server process
  inherited at *its* first spawn. Withholding therefore needs both halves:
  an `env -u` prefix on the launched command (covers the initial exec) AND
  `set-environment -r` on the session (covers every later
  `respawn-pane`/`split-window`, which take no env args). `-r` ≠ `-u`:
  unset merely re-exposes the global. For fly: whoever first starts the
  tmux server determines the baseline env of every future pane —
  **fly should start the server itself with a scrubbed environment**
  (the `CLAUDE_CODE_CHILD_SESSION` strip list moves to server spawn +
  per-session `-e`/`-r`), not rely on per-pane strips alone.
- **No re-negotiation on supervisor restart, because there is nothing to
  re-negotiate:** their agents call the `gc`/`gt` CLI, which resolves a
  path-stable store — no per-supervisor-instance endpoint is baked into
  session env. fly's current `FLY_SOCKET_PATH` is PID-keyed, i.e. exactly
  the thing that breaks when panes outlive the process. Strong evidence
  for D10's expected resolution: a **stable per-flavor socket path**, so a
  surviving agent's hooks reconnect to whichever fly instance currently
  binds it.

## D12 — Security

- **No signal**: no acknowledgment anywhere in the provider or SECURITY.md
  that same-uid tmux access (attach/send-keys) widens the local input
  surface. Adjacent practices worth noting: controller-only credentials
  are actively withheld from agent sessions (the two-half withholding
  above — their trust boundary runs *controller vs agent*, not *uid vs
  uid*), and session names are injection-validated before ever appearing
  in `run-shell` hook strings — fly must do the same wherever a leaf key
  or automation name is spliced into a tmux hook command.

## Spike shortcuts collected

tmux ≥ 3.2 floor (`-e`); behaviors falsified against 3.4 specifically;
`window-size latest` after every create; `exit-empty off`; `-u` always;
the ga-h9z degraded-server preflight; `set-hook client-attached` +
`wait-for -S` for event-driven attach readiness (upgrade over S4's polling);
`pane-died` hook + `remain-on-exit` (and respawn resetting it); per-window
not per-session activity; `send-keys -l` 4096 threshold + "not in a mode"
transient; SIGWINCH wake for detached TUIs; create-first TOCTOU dodge;
name charset `^[a-zA-Z0-9_-]+$`.

## Strategic asymmetry

Their `runtime.Provider` interface (create/kill/IsRunning/Send/Pending/
Respond/attach-state, backed by the TTL StateCache) is structurally the
substrate trait our D13 plans for `pty/` — evolved through k8s and ssh
providers without changing callers, which is the composability argument in
practice. Where shapes are otherwise tied, prefer substrate methods keyed by
(session name, store record) rather than in-process handles — that is what
would someday let fly project/attend an externally-managed tmux server.
Tiebreaker only; no speculative generality added for it.

## Brainstorm priors now believed wrong (or materially incomplete)

1. **D1's control-mode-forward hybrid is over-weighted.** Prior: control
   mode for events/lifecycle + capture-pane for unfocused mirrors. Evidence:
   Gas City runs full orchestration on subprocess polling + a 2 s TTL cache
   and event hooks (`pane-died`, `client-attached`) — no control mode at
   all, at fleet scale. Control mode's real earner in fly is only the
   *focused pane's live byte stream*. The plan should start
   subprocess+hooks-first and scope control mode to the focused-mirror
   stream, or even defer it (S2's 4 Hz capture loop may be enough for the
   focused mirror too, given typing happens in the attached terminal).
2. **S3 was incomplete, not wrong:** delivery fidelity passed against `cat`,
   which is not a raw-mode TUI. Two additions are load-bearing: the
   SIGWINCH wake for detached Claude, and post-Enter verified-submit
   (ga-bwm). Both must appear in the plan's delivery unit.
3. **D7 lacked two failure arms** the brainstorm didn't name: the ga-h9z
   degraded-server socket clobber (refuse-to-create guard required), and
   "observation failure ≠ empty roster" (destructive sweep arms defer).
4. **D2/D8 under-specified window geometry:** detached sessions pin to
   80×24 on tmux ≥ 3.3. Mirror rendering, screen-fallback parsing, and the
   attach experience all depend on fly actively managing per-session
   `window-size`/geometry. New requirement, previously invisible.
5. **D10's env model was too pane-local:** the tmux server's own
   environment is the inherited baseline for every pane; scrubbing must
   move to server spawn, and empty-vs-absent ("withhold" vs "unset")
   needs the two-half treatment if fly ever withholds an inherited var.
