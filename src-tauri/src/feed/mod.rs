//! The local, read-only agent/automation feed (feat-agent-state-local-feed).
//!
//! A narrowly-scoped, loopback-only realization of the browser-reachable HTTP
//! endpoint the hook socket deliberately deferred (`hooks/CLAUDE.md`, KTD7). It
//! lets an external local consumer (the `game` portfolio) *see* what agents are
//! running and what automations exist — never act on them.
//!
//! Data flow (see the plan's High-Level Technical Design): the webview PUSHES
//! its assembled agent roster (`publish`, U2), the backend merges it with
//! automations from the authoritative store and streams the result over SSE
//! (`server`, U3). The `wire` module is the single source of truth for the
//! boundary shape, mirrored by `src/lib/feed.ts`.

pub mod wire;
