//! Automations — cron-scheduled agent and script runs
//! (docs/plans/2026-07-01-002-feat-automations-plan.md).
//!
//! U1 ships the pure domain vocabulary ([`model`]), U2 the schedule math
//! ([`schedule`]), and U3 the write-through mutex-authority store
//! ([`store`]). The manager + sweep loop (U4) and the script/agent runners
//! arrive in later units and consume all three.

pub mod model;
pub mod schedule;
pub mod store;
