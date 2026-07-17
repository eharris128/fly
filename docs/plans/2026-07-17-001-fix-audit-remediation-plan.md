---
title: "fix: audit remediation — feed scrub parity, permission-gate coverage, and low-severity hardening"
type: fix
date: 2026-07-17
status: implemented
depth: standard
origin: full-codebase audit (four-agent security / backend-correctness / frontend / hygiene sweep, session 2026-07-17)
---

# fix: Audit remediation

## Summary

A full-codebase audit (2026-07-17) found no critical or high-severity issues:
the test baseline is green, docs have zero drift, and both trust boundaries
(hook socket, feed HTTP) implement their stated controls. It did find **two
genuine gaps in controls the code otherwise applies deliberately**, one build-
profile gap behind a known past bug class, and a tail of low-severity
robustness/hardening items. This plan resolves all of them.

The three headline items:

1. **Feed reply scrub parity (U1).** `GET /agents/{key}/output` serves the
   latest assistant reply (`text`) only control-sanitized — never through
   `redact::scrub_secrets` — while the sibling `turns` array and every question
   string go through the full `clean()` pipeline. The gap is *documented* as
   deferred parity in `feed/io.rs::resolve_io`'s doc comment; this plan closes
   the deferral. The reply is the single field most likely to quote an
   agent-echoed secret, and the feed is on by default (port 4939).
2. **Permission-gate coverage for unguarded submits (U2).** The
   `feed.allowPermissionAnswers` opt-in (default off) lives entirely inside the
   `if let Some(asked) = input.if_asked_at` guard block in
   `feed/server.rs::handle_input`. A `mode:"submit"` with no `ifAskedAt` skips
   it and delivers bracketed-paste + Enter while a permission dialog is up —
   the trailing Enter can confirm the dialog's default, the exact act the
   opt-in exists to gate. Today that's only *incidentally* blocked by the
   paste's leading ESC tending to cancel an unfocused picker (a Claude-UI
   artifact, not an enforced check).
3. **`overflow-checks` on the `release-dev` profile (U3).** Both release
   profiles wrap silently on integer overflow — the exact condition behind the
   recorded iso8601 parser bug, invisible to tests (dev/test profiles check).
   Enable checks on **`release-dev` only**: with `panic = "abort"`, enabling
   them on the shipping profile would turn any wrap into an abort that kills
   every live agent pane at once. `release-dev` is the local iteration profile
   (`pnpm build:local`) — exactly where the iso8601 bug would have been caught.
   Checked/bounded arithmetic on untrusted numeric input (already applied in
   `session/transcript.rs`, `usage/gate.rs`) remains the shipping defense.

The low-severity tail (U4–U13) covers: store-mutex poison recovery, fsync in
the atomic writers, connection caps on both servers, socket-dir permissions in
the temp fallback, the screen-replay allocation clamp, the Terminal spawn race
+ pane-id map pruning, a leader double-tap literal escape, `--session-id`
hygiene in resume argv, a webview CSP, and comment hygiene.

---

## Findings recap (what the audit established, verified in-tree)

| # | Sev | Where | Finding |
|---|-----|-------|---------|
| 1 | Medium | `feed/io.rs::resolve_io` | Reply `text` sanitized but never secret-scrubbed; `turns`/questions are. Doc comment acknowledges the asymmetry as deferred. |
| 2 | Low-Med | `feed/server.rs::handle_input` | Opt-in permission gate only runs under `ifAskedAt`; unguarded `mode:"submit"` bypasses it while an ask is pending. |
| 3 | Medium | `src-tauri/Cargo.toml` profiles | No `overflow-checks` on `release`/`release-dev`; known silent-wrap bug class; tests can't see it. |
| 4 | Low | `automations/store.rs` | Plain `.lock().unwrap()` — a panicked mutation closure poisons the mutex and the next sweep tick kills the sweep thread. Sibling `session/resume.rs` deliberately recovers poison with the identical on-disk-snapshot safety argument. |
| 5 | Low | `store.rs` / `resume.rs` / `alerts.rs` atomic writers | write-temp → chmod → rename with no fsync; `resume.rs` claims power-loss durability it can't guarantee on all filesystems. |
| 6 | Low | `feed/server.rs` + `hooks/server.rs` accept loops | One thread per connection, no cap; feed spawn precedes auth on a port any local uid can reach — local availability DoS. |
| 7 | Low | `lib.rs` socket-dir fallback | `XDG_RUNTIME_DIR` unset ⇒ socket dir under world-writable temp, created with default umask; predictable path ⇒ squat/bind-disruption DoS. |
| 8 | Low | `feed/screen.rs::ensure_row` | CSI `B`/`e`/`E` with param up to 65535 transiently allocates ~65k row Vecs before the drain caps at `MAX_GRID_ROWS`. |
| 9 | Low | `Terminal.svelte` onMount / `App.svelte` id maps | Async spawn has no destroyed guard (close-during-spawn orphans a PTY until shutdown reap); `paneIdByLeaf`/`leafByPaneId` are never pruned on close. |
| 10 | Low | `lib/keymap.ts` | Default Ctrl-A leader permanently shadows readline beginning-of-line; no way to send a literal leader. |
| 11 | Low | `lib/resume.ts` | User-supplied `--session-id` survives into resume argv alongside the appended `--resume` — self-conflicting invocation. |
| 12 | Low | `tauri.conf.json` | `"csp": null` — no backstop for any future `innerHTML` of untrusted content. |
| 13 | Low | `automations/script.rs` | Two stale-reading `TODO(U6)` markers describing completed work (the only TODO markers in the tree). |

Explicitly **accepted, not in scope** (see Out of scope): the same-uid trust
model, prefix-based secret scrubbing, the usage gate's lock-across-fetch
(single-consumer by design), the `automations/mod.rs` size, and shipping-
release overflow-checks.

---

## Key Technical Decisions

- **KTD1 — Reply scrub joins the one `clean()` pipeline; no parallel path.**
  The reply passes sanitize → scrub in `resolve_io` where `sanitize_multiline`
  runs today, preserving the contract order (sanitize *then* scrub, so
  control-char removal can't reassemble a split secret — the tested
  anti-reassembly argument in `feed/io.rs`/`redact.rs`). The reply keeps its
  existing no-truncation posture (truncation is the third stage and remains
  per-field; `turns`/questions keep their caps). Scrub happens at
  `ResolvedIo` build time, so the mtime/len cache stays valid. The
  "deferred parity" paragraph in the doc comment is deleted, and the
  CLAUDE.md feed section line updated — a secret-bearing final turn now reads
  `[redacted]` in **both** `text` and `turns`.
- **KTD2 — While a permission ask is pending, *all* PTY-writing input
  requires the `ifAskedAt` guard.** An unguarded `mode:"submit"` does a fresh
  gate + question read (the same body-independent re-read the guarded path
  does); if the resolved pending question is a permission ask (`kind ==
  "permission"`, or screen-derived under live reason `permission` — the same
  widened predicate the guarded path uses), it is refused **409** with body
  `{"error":"askPending"}`. 409-not-403 because the blocker is the missing
  guard, not the opt-in: with `ifAskedAt` supplied, the existing opt-in logic
  applies unchanged (a guarded submit is already gated — "a digit or a
  submit's Enter both confirm it"). This makes the opt-in airtight *and*
  keeps the stale-answer protection: there is no path to answer a permission
  dialog without both the freshness guard and (for permission asks) the
  config opt-in. A submit with no pending permission ask is untouched — the
  normal remote-instruction path stays unguarded.
- **KTD3 — Overflow checks are a `release-dev`-only net.** Shipping `release`
  semantics are deliberately unchanged (wrap remains preferable to an
  abort-storm under `panic = "abort"`); the profile comment is rewritten to
  state the intentional divergence, replacing "same semantics as release".
- **KTD4 — Poison recovery copies the `resume.rs` pattern, with the same
  justification.** Every `self.inner.lock().unwrap()` in `automations/
  store.rs` becomes `lock().unwrap_or_else(|p| p.into_inner())` behind one
  `fn lock_recovered(&self)` helper. Safe for the same reason `resume.rs`
  documents: the on-disk file is only ever a complete renamed snapshot, so an
  interrupted in-memory mutation can be continued from without propagating a
  torn state. The sweep thread must survive any panicking mutation closure.
- **KTD5 — fsync-before-rename, best-effort dir sync.** The two atomic
  writers (`store.rs::write_atomic_owner_only`, `resume.rs::write_records`)
  and the alerts-log startup truncation rewrite gain `sync_all()` on the temp
  file before rename plus a best-effort parent-directory fsync after. This
  makes `resume.rs`'s stated "power loss" durability claim true instead of
  weakening the claim; the files are small and writes are per-mutation, so
  the latency cost is negligible.
- **KTD6 — Connection caps as an RAII-counted accept guard.** Both accept
  loops get a shared `Arc<AtomicUsize>` with an RAII decrement guard; a
  connection arriving above the cap (const, 64 per server) is dropped
  immediately — before any read on the feed, before auth work on the hook
  socket. Availability guard only: 64 is far above any legitimate concurrent
  load (the feed has ~1 consumer; hooks are short-lived except held asks,
  which are bounded at 64 by `AskRegistry` anyway).
  *As built:* the hook socket drops silently as specified; the feed's
  refusal is an explicit body-less **503** on the accept thread — tiny_http
  auto-responds `500` to a dropped request, so a plain drop is not quieter,
  and the 503 is the minimal deliberate signal (`ConnCap`/`ConnSlot` live in
  `hooks/server.rs`, shared by both loops).
- **KTD7 — Socket dir is 0700 on every path, not just the fallback.**
  `DirBuilder` with mode 0700 (create) / explicit chmod (pre-existing dir)
  for `$XDG_RUNTIME_DIR/<app>` *and* the temp fallback — belt and braces:
  correct even if `XDG_RUNTIME_DIR` points somewhere non-default. The socket
  file's own 0600 + peercred check is unchanged.
- **KTD8 — Clamp before allocate in the grid replay.** `ensure_row` clamps
  `self.row` to `MAX_GRID_ROWS` *before* the row-push loop, making peak
  allocation O(`MAX_GRID_ROWS`) regardless of the CSI parameter. Semantics
  are unchanged — rows beyond the cap were already drained; this removes only
  the transient churn.
- **KTD9 — Spawn-race guard is a captured flag; map pruning is leaf-keyed.**
  `Terminal.svelte`'s async `onMount` captures a `destroyed` flag set by the
  cleanup; if set when `spawnPane` resolves, the component closes the fresh
  pane immediately and never calls `onSpawned`. `App.svelte` prunes
  `paneIdByLeaf`/`leafByPaneId` on every close path (pane, tab, workspace)
  by walking the closed subtree's leaf keys — extracted as a pure helper so
  it's vitest-testable.
- **KTD10 — Double-tap leader sends one literal leader, via `BINDINGS`.**
  Pressing the leader chord while `leaderPending` resolves to a
  `send-literal-leader` action (the leader combo's control byte written to
  the PTY) — the tmux `C-a C-a` convention. It is a `BINDINGS` entry like
  every other command, so the hotkey menu and palette pick it up without
  drift, and it works for any configured leader, not just Ctrl-A.
- **KTD11 — `--session-id` joins the resume strip list.** `sanitizeFlags`
  strips `--session-id` (separate-value and `=` forms) exactly like
  `--resume`/`--continue`: a replayed argv must never carry both a mint-new-id
  flag and an attach-existing-id flag.
- **KTD12 — CSP: restrictive, validated live.** `tauri.conf.json` gets a CSP
  (Tauri v2 auto-injects its own script nonces/hashes when one is set):
  `default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self'
  data:; worker-src blob:` as the starting point — `'unsafe-inline'` styles
  for xterm.js, `blob:` workers for the feed-poll worker
  (fix-feed-stale-status-while-backgrounded). Because WebKitGTK + the custom
  asset protocol has bitten before (the `crossorigin` incident), this unit is
  **gated on live validation** in a `release-dev` build: xterm renders, the
  worker poll ticks, no `[fly-webview]` CSP violations on stderr. If a
  directive can't be satisfied without neutering the policy, record what was
  tried and keep `null` with a doc note — don't ship a placebo CSP.
  *As built (validated 2026-07-17, release-dev under an isolated
  `FLY_APP_NAME=fly-smoke` flavor, GDK_BACKEND=x11):* the exact CSP above
  shipped; the full webview UI renders (dashboard, sidebar, usage gauges via
  Tauri IPC — Tauri v2 auto-injects its IPC/script amendments), the pane
  spawned its shell, and stderr carried **zero** `[fly-webview]` lines across
  two launches. Two sub-checks were not directly observable from outside the
  webview: the blob worker's tick (its CSP-failure path degrades silently to
  `setInterval` — but `worker-src blob:` explicitly allows it and no error
  surfaced) and the on-screen xterm paint (mounted and live behind the
  home-base dashboard, which by design is the first painted view; xterm uses
  no resource class the rendered UI didn't already prove).

---

## Requirements

- **R1** — Every feed-exposed string, including the reply `text`, passes
  sanitize → scrub before serving; a secret in the final assistant turn reads
  `[redacted]` in both `text` and `turns` of the same response.
- **R2** — No PTY-writing input path reaches a pane with a pending permission
  ask without `ifAskedAt`; the opt-in gate on guarded permission answers is
  unchanged. Non-permission submits are byte-identical to today.
- **R3** — Shipping `release` profile behavior is unchanged; `release-dev`
  builds abort on integer overflow.
- **R4** — A panicking store-mutation closure does not kill the sweep thread;
  the next tick proceeds against the recovered in-memory map.
- **R5** — The atomic writers fsync data before rename; the durability doc
  claims match the implementation.
- **R6** — Concurrent connection threads per server are bounded; over-cap
  connections are dropped without reading (feed) or auth work (hook socket).
- **R7** — The hook-socket parent dir is mode 0700 on every creation path,
  including the temp fallback.
- **R8** — Grid-replay memory is bounded by `MAX_GRID_ROWS` at all times,
  including mid-`ensure_row`.
- **R9** — Closing a pane/tab/workspace during the spawn window cannot leak a
  live PTY past the close; the pane-id maps hold entries only for live leaves.
- **R10** — Double-tapping the leader delivers exactly one literal leader
  keystroke to the PTY and is discoverable in the hotkey menu and palette.
- **R11** — A resume argv never contains `--session-id` alongside
  `--resume`/`--continue`.
- **R12** — The webview runs under a CSP that a `release-dev` build renders
  correctly under (xterm, feed worker, fonts) — or the `null` is kept with a
  recorded rationale (KTD12's escape hatch).
- **R13** — Every behavior change above ships with a test in the module's
  existing test style; CSP (R12) is validated live instead.
- **R14** — The stale `TODO(U6)` markers are reworded; no TODO/FIXME markers
  describe completed work.

---

## Units

Ordered; each unit is independently landable. Phase A is the security payload,
B the backend robustness tail, C the frontend/config tail.

**Phase A — the actionable three**

- **U1 — Reply scrub parity** (`feed/io.rs`, doc touch in `CLAUDE.md`).
  Route the reply through sanitize → scrub at `ResolvedIo` build (KTD1);
  delete the deferred-parity paragraph; update the CLAUDE.md feed section.
  Tests: secret-bearing final reply reads `[redacted]` in `text` and `turns`
  (extend the existing `redact.rs`/`io.rs` round-trip tests); blank-after-
  sanitize reply still counts absent; cache hit serves the scrubbed form.
- **U2 — Unguarded-submit permission gate** (`feed/server.rs`). Implement
  KTD2: fresh gate read for guardless submits, 409 `askPending` when the
  pending question is a permission ask (incl. screen-under-permission).
  Tests: unguarded submit vs pending hook ask → 409 (opt-in on *and* off —
  the guard, not the opt-in, is the blocker); unguarded submit with a
  pending *choice* question → delivered as today; guarded submit behavior
  unchanged (existing tests must not change).
- **U3 — `release-dev` overflow checks** (`src-tauri/Cargo.toml`). Add
  `overflow-checks = true` to `[profile.release-dev]`; rewrite the profile
  comment per KTD3. Validation: `pnpm build:local` compiles; a deliberate
  local overflow repro aborts under `release-dev` and wraps under `release`
  (spot-check, not committed).

**Phase B — backend robustness**

- **U4 — Store poison recovery** (`automations/store.rs`). `lock_recovered`
  helper per KTD4, applied at every lock site; doc comment cites the
  `resume.rs` precedent and the snapshot safety argument. Test: poison the
  mutex via `catch_unwind` around a panicking `mutate` closure, then assert
  the next `mutate`/`snapshot` succeeds.
- **U5 — fsync the atomic writers** (`store.rs`, `session/resume.rs`,
  `automations/alerts.rs`). KTD5. Tests: existing round-trip/perms tests
  keep passing (fsync itself isn't unit-assertable; the unit is small enough
  that review + green suite is the check).
- **U6 — Connection caps** (`feed/server.rs`, `hooks/server.rs`). KTD6:
  shared counter + RAII guard, cap 64, drop-over-cap before read/auth.
  Tests: open cap idle connections against a test server, assert connection
  cap+1 is closed immediately and a slot frees when one closes.
- **U7 — Socket dir 0700** (`lib.rs`, `hooks/server.rs`). KTD7 via
  `std::os::unix::fs::DirBuilderExt`. Test: create through the helper into a
  tempdir, assert mode 0700 on the created dir (both the runtime-dir shape
  and the temp-fallback shape).
- **U8 — Grid-replay clamp** (`feed/screen.rs`). KTD8 one-line clamp in
  `ensure_row`. Test: replay `ESC[65535B` (and `E`/`e` variants), assert
  grid height ≤ `MAX_GRID_ROWS` and the parse still abstains/renders as
  before on the existing fixtures.

**Phase C — frontend + config**

- **U9 — Spawn-race guard + id-map pruning** (`lib/Terminal.svelte`,
  `App.svelte`, new pure helper + test). KTD9. Tests: pure pruning helper
  over a closed subtree (pane/tab/workspace shapes); the destroyed-guard
  logic extracted far enough to assert "resolve after destroy ⇒ close, no
  onSpawned" without mounting a component.
- **U10 — Leader double-tap literal** (`lib/keymap.ts`, `BINDINGS`). KTD10.
  Tests in `keymap.test.ts`: leader,leader ⇒ literal-send action + pending
  cleared; works under a remapped leader; menu/palette derive the entry.
- **U11 — Resume `--session-id` strip** (`lib/resume.ts`). KTD11. Tests:
  both flag forms stripped; interaction with the existing `--resume` strip
  and `--add-dir` variadic preserved.
- **U12 — CSP** (`src-tauri/tauri.conf.json`). KTD12, gated on live
  validation in a `release-dev` build (xterm render, worker poll, stderr
  clean). Deliverable is either the CSP or a recorded-why `null` — not a
  silent skip.
- **U13 — Comment hygiene** (`automations/script.rs`, `usage/gate.rs`).
  Reword the two `TODO(U6)` markers to plain notes (R14); confirm the
  usage-gate single-consumer lock invariant note still reads correctly after
  U5/U6 land nearby (no code change expected).

**Final gate** (after C): `cargo test --offline`, `pnpm check`,
`pnpm test:unit`, then a `pnpm build:local` smoke: app renders (CSP), feed
probe over `curl 127.0.0.1:4939` shows a scrubbed reply, an unguarded submit
against a synthetic pending ask 409s.

---

## Out of scope (deliberate)

- **Shipping-`release` overflow-checks** — rejected per KTD3 (abort-storm
  trade-off); revisit only with a panic-isolation story.
- **Same-uid trust model** (peercred is uid-only; pane tokens readable from
  `/proc/<pid>/environ` by same-uid) — the documented desktop trust boundary,
  reaffirmed by the audit, unchanged.
- **Entropy/prose secret detection in `redact.rs`** — best-effort
  prefix/marker scrubbing is the designed posture; U1 extends *where* it
  applies, not *what* it catches.
- **Usage-gate lock-across-fetch** (`usage/gate.rs`) — safe with its single
  sweep consumer and documented as such; restructure only when a second
  consumer appears.
- **Splitting `automations/mod.rs` (~6k lines)** — size-only concern, no
  correctness impact; defer until it next grows.
- **Pane-mode at-limit close honesty** — already deferred as U6 of the
  usage-limit-deferral plan behind its empirical checklist; not re-scoped
  here.
