//! Authenticated agent-hook channel (U8, R10).
//!
//! A Unix-domain socket plus per-pane CSPRNG tokens: the PTY is a trust
//! boundary, so an unauthenticated endpoint would let any local process spoof
//! attention or enumerate panes (KTD7). See [`protocol`] for the wire spec.

pub mod protocol;
pub mod server;
pub mod token;

pub use protocol::HookMessage;
pub use server::{
    AskHandler, AskTicket, Dispatch, HookServer, PeerHandler, RequestHandler, SubstrateHandler,
    ValidatedHook,
};
pub use token::TokenRegistry;
