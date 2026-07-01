//! Automations — cron-scheduled agent and script runs
//! (docs/plans/2026-07-01-002-feat-automations-plan.md).
//!
//! U1 ships the pure domain vocabulary ([`model`]) and U2 the schedule math
//! ([`schedule`]). The store (U3), manager + sweep loop (U4), and the
//! script/agent runners arrive in later units and consume both.

pub mod model;
pub mod schedule;
