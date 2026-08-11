//! The session substrate: fly-managed tmux server + sessions.
//!
//! Plan: `docs/plans/2026-08-11-001-feat-tmux-session-substrate-plan.md`.
//! This module is U1 — the tmux wrapper with a pure, executor-seamed core so
//! argument construction and error classification are unit-tested without a
//! tmux binary (the Gas City provider shape; see the 2026-08-11 reference-
//! mining note). Nothing here is reachable from the app until U3 switches the
//! spawn path behind KTD10's `substrate` config flag.
//!
//! Boundaries (KTD3/KTD4): fly owns the per-flavor server (`-L` socket) and
//! only ever operates on sessions it created and marked by name. The wrapper
//! refuses invalid names outright — tmux target metacharacters (`.`,`:`)
//! silently misroute rather than erroring.

pub mod naming;
pub mod tmux;

pub use naming::{leaf_session_name, session_leaf_slug, validate_session_name};
pub use tmux::{Executor, RealExecutor, Tmux, TmuxConfig, TmuxError};
