//! Per-pane state machines (KTD8).
//!
//! Two orthogonal machines model a pane: [`lifecycle`] (process status, fed by
//! PTY events) and the attention machine (added in U7). U2 only produces
//! lifecycle transitions; U7 formalizes the full transition tables, the
//! attention machine, and the notification-suppression matrix.

pub mod attention;
pub mod lifecycle;
pub mod manager;
pub mod suppress;

pub use manager::AttentionManager;
