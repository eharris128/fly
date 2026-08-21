# The fly core control protocol

Wire contract for the Electron shell migration
(`docs/plans/2026-08-12-002-proposal-electron-shell-migration-plan.md`, U1;
KTD1/KTD3). Implemented in `src-tauri/src/control/`. This document and that
module are edited only together.

## Transport

A Unix stream socket at `$XDG_RUNTIME_DIR/<flavor>/control.sock` (flavor =
`lib.rs::app_dir_name()`, so a dev flavor gets its own socket beside its own
`hook.sock`). The directory is 0700, the socket 0600, and every accepted
connection is checked for **same-uid peer credentials** (`SO_PEERCRED` /
`getpeereid`) before any byte is read — the hooks-socket discipline, reused.
The bind **never steals a live socket**: if the path answers a connect, the
bind fails `AddrInUse`; only a dead (crash-residue) socket is reclaimed.

There is no token: unlike the hook socket (whose clients prove *which pane*
they are) the control socket's client is the shell/renderer acting as the
user; same-uid is the boundary, exactly as it is for the tmux server and the
hook socket's peer-cred floor.

## Framing

Every message in either direction is one frame:

```
u32 LE payload length | u8 kind | payload
```

Length covers the payload only (not the kind byte). Frames above
`MAX_FRAME` (8 MiB) are refused **before allocation** — the reader drops the
connection rather than sizing a buffer from a peer-controlled number (the
`equal_reader` lesson; also the release-overflow-checks rule: the length is
bounds-checked as untrusted numeric input).

Kinds:

| kind | name | payload | direction |
|---|---|---|---|
| `0x01` | JSON | UTF-8 JSON, one envelope (below) | both |
| `0x02` | pane output | `u64 LE paneId` + raw PTY bytes | server → client |
| `0x03` | pane input | `u64 LE paneId` + raw keystroke bytes | client → server |

Kinds `0x02`/`0x03` exist so PTY bytes never touch JSON (proposal KTD3 — this
replaces both the Tauri `Channel` and the old eval/number-array quirk, and
gives keystrokes a JSON-free path down as well). Unknown kinds drop the
connection (version-skew fail-closed).

## JSON envelopes

Requests are client → server; responses and events are server → client. All
field names camelCase (the repo-wide serde convention — this is a wire
contract like `automations/model.rs`).

```jsonc
// request           // response (exactly one per request id)
{"id": 7,            {"id": 7, "ok": <result>}     // success
 "cmd": "spawn_pane",{"id": 7, "err": "message"}   // failure
 "args": { ... }}

// event (unsolicited, fan-out to every connected client)
{"event": "pane://attention", "payload": { ... }}
```

- `id` is client-chosen, opaque to the server, echoed verbatim; the client is
  responsible for uniqueness among its in-flight requests.
- `cmd` names are **exactly** today's Tauri command names (proposal KTD1);
  `args` is the same serde shape the command takes today. Porting a command
  must not rename anything.
- `event` names are exactly today's event names (`pane://…`,
  `automation://…`).
- A malformed JSON frame or an unknown `cmd` yields `{"id": …, "err": …}`
  when an id could be parsed, else the connection is dropped.

Two built-in commands exist. `core/ping` (U1) →
`{"pong": true, "version": "<crate version>"}` — the liveness/version probe a
shell uses at startup (and what the never-steal connect check trips on).
`core/shutdown` → `{"shuttingDown": true}` — triggers the backend's ordered
shutdown (`backend::ordered_shutdown`: clean-exit marker, sweep stop,
automations/feed/ask-registry shutdown, then `PtyManager::close_all`). The
Electron shell sends it on `before-quit` and waits for the core process to
exit (SIGTERM if the command fails, SIGKILL after a 10 s deadline). A host
built without a shutdown trigger answers it with an error rather than
ignoring it. The
`core/` prefix is reserved for protocol-level commands; ported app commands
keep their bare names.

## Concurrency & delivery

- Multiple clients may connect (cap: 64, shared `ConnCap` discipline).
  Responses go only to the requesting client; events and pane-output frames
  broadcast to every client.
- Writes carry a short timeout; a client that stops reading is dropped rather
  than wedging the broadcaster (hook-socket write-timeout rule).
- Frame order is preserved per connection (it's one stream); ordering across
  a response and a broadcast is not otherwise specified.
- Requests dispatch on the connection's read thread in U1 (one in-flight
  request per client at a time); if a ported command needs long-running work
  it must spawn internally rather than block the read loop.
