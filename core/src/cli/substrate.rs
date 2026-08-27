//! `fly substrate-event` — tmux-hook event reports (tmux plan U4b/KTD12).
//!
//! Invoked by tmux `run-shell` hooks (`pane-died`, `client-attached`/
//! `-detached`) armed on fly's marked sessions. Runs in the tmux SERVER's
//! context, whose environment carries the server-scope `FLY_SUBSTRATE_TOKEN`
//! and the stable `FLY_SOCKET_PATH` (injected at server spawn). Fire-and-
//! forget with silent exits throughout: a hook's stderr goes nowhere useful,
//! a missing fly is not the hook's problem, and the panes_status backstop
//! covers every lost event.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::hooks::protocol::SubstrateEvent;

/// `fly substrate-event <kind> <session> <arg>` where kind ∈
/// {`pane-died`, `attach-state`}; arg is `#{pane_dead_status}` or
/// `attached`/`detached`. Always exits 0 (hook context).
pub fn run(args: &[String]) {
    let (Some(kind), Some(session), Some(arg)) = (args.first(), args.get(1), args.get(2))
    else {
        return;
    };
    let (Ok(token), Ok(socket)) = (
        std::env::var("FLY_SUBSTRATE_TOKEN"),
        std::env::var("FLY_SOCKET_PATH"),
    ) else {
        return;
    };
    let status = match kind.as_str() {
        "pane-died" => arg.parse::<i32>().ok(),
        "attach-state" => Some(i32::from(arg == "attached")),
        _ => return,
    };
    let event = SubstrateEvent {
        token,
        op: "substrate/event".into(),
        kind: kind.clone(),
        session: session.clone(),
        status,
    };
    let Ok(bytes) = serde_json::to_vec(&event) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.write_all(&bytes);
    // EOF framing: closing the write side ends the request.
    let _ = stream.shutdown(std::net::Shutdown::Write);
}
