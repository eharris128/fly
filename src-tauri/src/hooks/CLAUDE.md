# hooks/ — the socket security boundary

This directory is fly's **trust boundary**. Read this before changing anything
here, then see the module doc-comments for detail. The root `CLAUDE.md` covers
the rest of the app; this note is scoped to the socket.

IDs below are scoped to four plans (per-plan numbering — see
`docs/plans/README.md`): the foundation plan
`2026-06-16-001-feat-fly-agent-terminal` (`KTD7`/`U8`/`R10`), the automations
plan `2026-07-01-002-feat-automations` (`U9`/`R22`), the hook-ask-channel
plan `2026-07-11-002-feat-hook-ask-channel` (the held `ask/hold` op), and the
agent-peer-messaging plan `2026-08-07-001-feat-agent-peer-messaging` (the
`peer/*` ops).

## What it defends

`fly notify` (and `fly automation …`) reach the running app over a
**Unix-domain socket** — never TCP. (The loopback-HTTP endpoint `KTD7`
originally deferred has since shipped as the *separate* `feed/` surface —
bearer-token-authenticated, loopback-only, its own boundary; the hook socket
itself remains Unix-only.) The PTY runs untrusted agent output and any
local process can try to connect, so an unauthenticated endpoint would let an
attacker spoof attention, enumerate panes, or drive automations. Authentication
here is mandatory, not a nicety.

## Invariants that must not regress

- **Per-pane CSPRNG token** (`token.rs`) — ≥128-bit (currently 256-bit),
  compared in **constant time** (`subtle::ConstantTimeEq`). Never log a token;
  never compare with `==`.
- **Peer-UID check** (`server.rs`) — the connecting peer's UID must equal the
  app's UID via `SO_PEERCRED` (Linux) / `getpeereid(2)` (macOS — same
  kernel-attested peer euid). Reject otherwise, on error too.
- **Lockout** (`token.rs`) — repeated invalid presentations trip a registry-wide
  cooldown (`MAX_FAILURES` / `LOCKOUT`) to blunt brute-force and spam.
- **Bounded I/O** (`server.rs`) — cap a single request (`MAX_MESSAGE`, 64 KiB)
  and apply read/write timeouts, so a peer can't stream forever or wedge a
  handler thread by connecting and never reading.
- **Silent rejection** — unknown, missing, or malformed tokens are rejected with
  **no signal**: don't leak whether a pane exists or why a message failed.
- **Capture-only messages** (`capture_only` / `SessionStart`, the
  session-pane-attribution plan) ride this **same authenticated socket** — no new
  route. The flag is read only *after* token validation, and a message can never
  select its own trust rank: rank is assigned downstream at the dispatch call
  site (a socket hook is always `Hook`), never from the wire payload. A forged
  in-pane capture is thus pane-precise but can't outrank or clear a human `Pick`.
- **Recursion origin** (`R22`) — an `automation/*` request's origin pane is the
  **authenticated token's pane**: resolved by the socket's token→pane validation
  (`server.rs`) and handed to the request handler, never read from the wire
  payload (`Envelope` carries only `token` + `op`; `Envelope::is_automation`
  merely routes by op prefix, so a client cannot claim a different origin). The
  actual recursion **gate** lives downstream in `automations/mod.rs` (the
  recursion registry — `is_automation_pane`), which blocks an automation-spawned
  pane from creating or running automations. Keep the origin the token-resolved
  pane so that gate can do its job.
- **Peer-op origin is token-resolved** (agent-peer-messaging `KTD2`) — a
  `peer/send`'s sender is the authenticated token's pane, exactly like the
  automation `R22` origin above: `PeerRequest` carries only the *target* pane
  + message, never a "from", so a client cannot impersonate another pane to a
  recipient (the delivered provenance frame is composed from the resolved
  origin). Skew rule: `PeerRequest` must never gain a field named `reason` —
  on an old server the op falls through to notify and that absence is what
  makes the fallthrough parse fail silently. The consent gate (`peerOptIn`)
  is deliberately **not settable over this socket** — it reaches the backend
  only via the webview's roster push.
- **Held asks stay bounded** (hook-ask-channel `R2`/`KTD1`) — the `ask/hold` op
  holds its connection for the ask's lifetime, so the pre-validation phase now
  has a hard wall-clock deadline (`REQUEST_DEADLINE`) on top of the size bound
  (newline framing never EOFs — a byte-trickler must not wedge a thread), held
  connections are capped upstream (`feed/ask.rs::MAX_HELD_ASKS`, decline =
  close-without-ack), and messages are **one line** of compact JSON (an
  embedded newline truncates → silent reject). Token validation still precedes
  any held work; a held connection can only ever *receive* one decision line —
  nothing a peer writes after its request is parsed.
  **Known broken upstream (Claude Code 2.1.224):** the clearing half of this
  rests on Claude killing the hook when a dialog is answered locally, and it
  no longer does — the hook lives on (≥250s measured, both dialog kinds), so
  the held ask never clears and every question-gated surface refuses forever.
  Diagnosis, measurements, and candidate fixes:
  `docs/notes/2026-08-07-peer-messaging-live-check.md`. Re-verify this contract
  whenever the Claude Code version moves.

## Layout

- `token.rs` — `TokenRegistry`: mint / resolve tokens, constant-time compare,
  lockout.
- `protocol.rs` — the wire schema (one UTF-8 JSON object per message: the
  EOF-framed `notify` path + `automation/*` envelope, `U9`, the
  newline-framed held `ask/hold` op, and the EOF-framed `peer/*`
  request/response ops). Its doc-comment is the authoritative schema +
  rejection-rule spec.
- `server.rs` — `HookServer`: the accept loop, `SO_PEERCRED`, bounds/timeouts,
  dispatch into `AttentionManager` / the automation handler / the peer
  handler, and the held-ask connection loop (`hold_ask` — ack, park on the
  decision mailbox, probe for peer death).

## Testing

The boundary has an integration test — run it after any change here:

```bash
cargo test --offline --manifest-path src-tauri/Cargo.toml --test hook_auth
cargo test --offline --manifest-path src-tauri/Cargo.toml --test hook_ask
cargo test --offline --manifest-path src-tauri/Cargo.toml --test peer_send
```

Add cases whenever you touch the token compare, peer-cred check, lockout,
message bounds, the held-ask framing, or the peer-op routing/origin — these
are the parts that must not silently weaken.
