# tmux substrate — Stage 1 spike results & Stage 2 decision memos — 2026-08-11

Companion to `docs/brainstorms/2026-08-11-tmux-session-substrate-requirements.md`
(the D-numbers below) and
`docs/notes/2026-08-11-gascity-tmux-reference-mining.md` (the external scar
tissue). This note is the direct input to the plan doc; each memo is a
KTD-in-waiting. Environment: tmux 3.4, claude 2.1.228, this box.

## Stage 1 results

**S1 — hook/env compat: PASS.** A tmux session created with
`new-session -e FLY_PANE_TOKEN=… -e FLY_SOCKET_PATH=…` (borrowed from a live
fly pane) completed a full authenticated request/response over the hook
socket from inside the pane (`fly agents` → real roster reply) and an
accepted `fly notify permission --claude` dispatch (exit 0). Env passthrough
verified in-pane. The `--capture` arm was deliberately not fired against the
production instance (it would upsert a fake session id into the real resume
store); it rides the same auth path.

**S2 — mirror bake-off: PASS, decisively.** 5 tmux sessions × 33 Hz spinner,
mirrored into a WebKitGTK page via `capture-pane` polling + JS eval
(deliberately the Tauri-channel shape), webview main thread sampled at 100 ms
(protocol of `2026-08-11-webkitgtk-engine-floor.md`; same stimulus measured
**63% avg / 87% busy** in fly's xterm.js panes):

| mirror mode                       | avg  | peak | >50% busy |
|-----------------------------------|------|------|-----------|
| textContent, 4 Hz                 | 4.0% | 20%  | 0%        |
| innerHTML spans (worst), 4 Hz     | 27.7%| 90%  | 27%       |
| innerHTML spans (worst), 2 Hz     | 14.1%| 90%  | 17%       |

"Worst" = every word its own colored span, full innerHTML replace per tick,
no diffing. The < 20% architecture target is met at 2 Hz even at worst-case
density; realistic density and per-line diffing land far under it. Driver
(python capture+convert) 3.8%, tmux server 4.2% — both trivially cheap, and
the fly-side half will be Rust.

**S3 — input fidelity + delivery: PASS,** completed against a real detached
claude 2.1.228 (env scrubbed at the pane command per the Gas City overlay
lesson — the spike server had inherited this shell's `CLAUDECODE` markers,
proving the trap is real):
- Trust-dialog picker answered by plain `send-keys Enter`, **no SIGWINCH wake
  needed** — 2.1.228 processes detached stdin fine. Gas City's wake is
  version/provider scar tissue; keep it as cheap conditional insurance
  (`WakePaneIfDetached` before delivery), don't depend on it.
- `load-buffer` + `paste-buffer -p -d`: multiline prompt landed as ONE
  composer draft (no per-newline submits), UTF-8/quotes intact (earlier
  `cat -v` leg: no bracket markers delivered to a non-paste-mode reader,
  markers correctly delivered to one that enabled it — strictly better than
  fly's current unconditional bracketed paste).
- **Verified submit**: Enter → busy indicator ("esc to interrupt") observed
  on the first 250 ms poll; reply received; ga-bwm's drop race did not
  reproduce but the confirm loop cost one poll — it stays.

**S4 — lifecycle round-trip: PASS.** `client-attached`/`client-detached`
hooks fire on a scripted-pty attach/detach (the `script -qfc` trick — which
Gas City independently productized as their hidden-attach client); sessions
and running processes survive client churn and fly restarts (the flyspike
server outlived three fly relaunches during these spikes). Residual: the
hook's session-name format var came back empty — resolve the right
`#{…}` spelling during build; and prefer Gas City's `wait-for -S` channel
pattern over polling for attach readiness.

## Stage 2 decision memos (→ plan KTDs)

**D1 — Observation: subprocess polling + tmux event hooks; NO control mode.**
Lifecycle/state: batched `list-sessions`/`list-panes -F` polls behind a
short-TTL cache (fly already has the poll-batching shape), plus event hooks —
`pane-died` for exit (with `remain-on-exit on`, re-armed after every respawn,
feeding the lifecycle state machine via a `fly`-CLI hook command over the
socket), `client-attached`/`client-detached` for attach state. Activity reads
use per-window `#{window_activity}` (session-level freezes when detached)
with an own-input discount à la Gas City's poke echo. Control mode is
dropped from v1 entirely — Gas City runs fleets without it, and our one
live-stream consumer is served by D2's pipe-pane.

**D2 — Mirror tiers.** (a) *Focused in-window pane*: live bytes via
`pipe-pane -o` into fly → the existing coalesce → channel → xterm.js path,
i.e. today's machinery scoped to exactly one pane (~63%/5 ≈ low-teens %,
tolerable). (b) *Visible-unfocused*: `capture-pane -e` snapshots at 2 Hz
rendered as styled DOM — S2 bounds worst case at 14%. (c) *Hidden*: no
standing observation; on-demand capture (the feed's screen fallback becomes
a capture-pane call — better than today's ring+vte replay, and the vte grid
retires). fly drives each session's `window-size` to the fly pane grid
(tmux ≥ 3.3 pins detached sessions at 80×24 otherwise) and re-drives on
resize; `window-size latest` set at create so an attached client wins.

**D3 — tmux required.** No dual substrate. Startup probes `tmux -V`
(floor 3.2 for `-e`; validated on 3.4) and refuses with an install hint.
The portable-pty path is removed at parity, not maintained behind a flag
past the rollout window.

**D4 — Identity.** Session name = `fly-<flavor>-<leafKey-slug>`, charset
`^[a-zA-Z0-9_-]+$` enforced (dots/colons are tmux target metacharacters);
the sluggification must be injective per store. fly's session store remains
authoritative; discovery = store record × `has-session`. **Never adopt
unmarked sessions.** Unlike Gas City, a marked session whose agent died is
NOT auto-killed — it surfaces as an exited pane (fly is a terminal; the
user may want the final screen); kills happen only through fly's own close
paths.

**D5 — Input.** Ladder: `send-keys -l` for ≤ 4096 bytes, else
`load-buffer` + `paste-buffer -p -d`; retry-with-backoff on the transient
"not in a mode" startup error; `WakePaneIfDetached` (resize dance) before
programmatic delivery as version insurance; **verified submit** on every
delivery route that ends in Enter (busy-poll, re-Enter only while idle,
unconfirmed surfaces as a delivery refusal — never a silent success);
per-session delivery serialization (the existing app-wide delivery mutex
narrows to per-session locks). In-window keystrokes for the focused pane:
`send-keys -l` per write-chain flush (order preserved by the existing
per-pane chain).

**D6 — Focus/suppression.** `#{session_attached}` + the attach hooks feed
the replicated focus tuple: an externally-attached session counts as
*focused* for `state/policy.rs` (a raise while the user is typing in the
attached terminal must suppress OS notification exactly like an in-window
focused pane). Detach restores normal suppression state.

**D7 — Lifecycle.** Before ANY `new-session`: the ga-h9z degraded-server
preflight (bounded `has-session` probe against an unrouteable name; refuse
on anything but a clean "not found"/"no server"). Server spawned by fly
with `exit-empty off`, `-u`, and a scrubbed environment (see D10). Startup:
reattach = store × `has-session`; sessions found dead → exited-pane
surface; **observation failure (server unreachable, probe timeout) defers
every destructive arm** — it is never "no sessions". Quit = detach by
default (agents keep running; the existing busy-agent destructive-confirm
gains a "quit and kill all" variant). The clean-exit marker's meaning
inverts accordingly.

**D8 — Scrollback.** tmux `history-limit` (config knob, default matching
today's 10k) becomes the store; per-leaf scrollback files retire; mirror
attach/reveal replays via `capture-pane -S`. The 64 KiB tail ring and the
vte grid replay retire in favor of direct capture (D2c).

**D9 — Backpressure.** The coalescer + watermarks survive only for the one
focused pipe-pane stream; tmux buffers everything else. Watermark constants
re-derived for the single-pane case in the plan.

**D10 — Env/token.** The hook socket path loses its PID key: stable
per-flavor `$XDG_RUNTIME_DIR/<app>/hook.sock`, so agents that outlive a fly
process reconnect to whichever instance currently binds it. Per-session
CSPRNG token injected via `-e` at create and persisted (encrypted at rest
not required — same trust domain as today's env) in the session store; a
restarted fly re-registers surviving sessions under their stored tokens.
The `CLAUDE_CODE_CHILD_SESSION`/`CLAUDECODE` strip moves to **server
spawn** (fly starts the tmux server, so the global-env baseline is clean),
with per-session `-e` overlays; any future *withholding* of an inherited
var uses the two-half `env -u` + `set-environment -r` pattern.

**D11 — Attach UX.** New chord (and dashboard verb) launches
`<config.terminal>` (default: `x-terminal-emulator`, knob in config) running
`tmux -L <flavor-sock> attach -t <session>`; the `client-attached` hook
flips the mirror to an "attached elsewhere" badge + suppression state.
Recommendations for the open product questions: in-window typing stays
fully functional (focused pane is a real live terminal); dashboard jump
stays in-window with attach one chord away; quit defaults to detach.

**D12 — Security.** `src-tauri/src/hooks/CLAUDE.md` gains the explicit
note: any same-uid process can `tmux attach`/`send-keys` into fly's server —
formally wider than today's fd-scoped PTY writes, unchanged in substance
(same-uid could already borrow tokens; see dev-flavor techniques memory).
Every string fly splices into a tmux hook command (leaf keys, automation
names) is injection-validated against the D4 charset first.

**D13 — Blast radius.** Stage 3 walks the module map; preliminary:
*simplified* — `session/resume.rs` (reattach-first), crash offer,
scrollback files, screen-fallback ring + vte; *adapted* — `pty/` → substrate
trait (session-name + store-record keyed, per the Gas City provider shape),
`stream/` (pipe-pane ingest + snapshot channel), delivery routes
(`feed/drop`, `peer/`, handoff injection) onto D5, automations `--paned`
dispatch, `lifecycle.rs` (detach-not-reap); *untouched* — `hooks/` wire,
`state/` machines, `feed/` wire contracts, dashboards, keymap (+2 chords),
automations headless path.

## Residuals carried to the plan

- `#{hook_session}` format spelling in attach hooks (S4 residual).
- The injective leafKey→session-name slug (leaf keys may hold chars outside
  the tmux-safe charset).
- Verified-submit's busy matcher must reuse/share `feed/screen.rs`'s picker
  awareness so "busy" vs "dialog waiting" can't be conflated.
- Cold-boot resume (server gone after reboot) keeps today's `--resume`
  flow; the plan needs the decision tree for "store says session, server
  says no server".
