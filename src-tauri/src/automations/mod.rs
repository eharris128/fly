//! Automations — cron-scheduled agent and script runs
//! (docs/plans/2026-07-01-002-feat-automations-plan.md).
//!
//! U1 ships only the pure domain vocabulary ([`model`]). The schedule math
//! (U2), store (U3), manager + sweep loop (U4), and the script/agent runners
//! arrive in later units and all consume these types.

pub mod model;
