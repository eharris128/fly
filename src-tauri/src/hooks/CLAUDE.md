# hooks/ — the socket security boundary

This directory is fly's **trust boundary**. Read this before changing anything
here, then see the module doc-comments for detail. The root `CLAUDE.md` covers
the rest of the app; this note is scoped to the socket.

IDs below are scoped to two plans (per-plan numbering — see
`docs/plans/README.md`): the foundation plan
`2026-06-16-001-feat-fly-agent-terminal` (`KTD7`/`U8`/`R10`) and the automations
plan `2026-07-01-002-feat-automations` (`U9`/`R22`).

## What it defends

`fly notify` (and `fly automation …`) reach the running app over a
**Unix-domain socket** — never TCP; the browser-reachable loopback-HTTP endpoint
is deliberately deferred (`KTD7`). The PTY runs untrusted agent output and any
local process can try to connect, so an unauthenticated endpoint would let an
attacker spoof attention, enumerate panes, or drive automations. Authentication
here is mandatory, not a nicety.

## Invariants that must not regress

- **Per-pane CSPRNG token** (`token.rs`) — ≥128-bit (currently 256-bit),
  compared in **constant time** (`subtle::ConstantTimeEq`). Never log a token;
  never compare with `==`.
- **Peer-UID check** (`server.rs`) — the connecting peer's UID must equal the
  app's UID via `SO_PEERCRED`. Reject otherwise.
- **Lockout** (`token.rs`) — repeated invalid presentations trip a registry-wide
  cooldown (`MAX_FAILURES` / `LOCKOUT`) to blunt brute-force and spam.
- **Bounded I/O** (`server.rs`) — cap a single request (`MAX_MESSAGE`, 64 KiB)
  and apply read/write timeouts, so a peer can't stream forever or wedge a
  handler thread by connecting and never reading.
- **Silent rejection** — unknown, missing, or malformed tokens are rejected with
  **no signal**: don't leak whether a pane exists or why a message failed.
- **Recursion origin** (`R22`) — the `automation/*` envelope carries its origin
  pane (`protocol.rs`, `Envelope::is_automation`); the actual recursion **gate**
  lives downstream in `automations/mod.rs` (the recursion registry —
  `is_automation_pane`), which blocks an automation-spawned pane from creating or
  running automations. Keep the origin faithfully stamped here so that gate can
  do its job.

## Layout

- `token.rs` — `TokenRegistry`: mint / resolve tokens, constant-time compare,
  lockout.
- `protocol.rs` — the wire schema (one UTF-8 JSON object, client closes its
  write half; the `notify` path + the `automation/*` envelope, `U9`). Its
  doc-comment is the authoritative schema + rejection-rule spec.
- `server.rs` — `HookServer`: the accept loop, `SO_PEERCRED`, bounds/timeouts,
  and dispatch into `AttentionManager` / the automation handler.

## Testing

The boundary has an integration test — run it after any change here:

```bash
cargo test --offline --manifest-path src-tauri/Cargo.toml --test hook_auth
```

Add cases whenever you touch the token compare, peer-cred check, lockout, or
message bounds — these are the parts that must not silently weaken.
