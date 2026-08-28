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

/// `fly substrate-pipe <fifo>` — the `pipe-pane` consumer (spike
/// 2026-08-28-001 KTD1). tmux hands the pane's output to this process on
/// stdin; it copies it into fly's per-pane FIFO with a plain `read`/`write`
/// loop and nothing else.
///
/// Why fly owns this instead of `cat > fifo`: a `cat` that copies with
/// `splice(2)`/`sendfile(2)` (uutils, busybox — the default `cat` on Ubuntu
/// ≥ 25.10) makes the kernel take the FIFO's pipe mutex and then sleep on the
/// socket *while holding it*, so fly's `read()`/`close()` on the FIFO block
/// uninterruptibly until the pane's next byte: every echo was released by
/// the next keystroke, and a dead pane's exit never surfaced. A read/write
/// copier never holds the pipe lock across a wait. For the same reason this
/// must never become `std::io::copy`, whose Linux specialization tries
/// `copy_file_range` → `sendfile` → `splice` on fd pairs.
///
/// Exits 0 on stdin EOF (tmux closed the pipe), on any write error (fly tore
/// the FIFO down first — Rust ignores SIGPIPE, so that is `EPIPE` here), or
/// when the FIFO cannot be opened; never logs — a pipe child's stderr goes
/// nowhere useful, and the `panes_status`/hook paths cover a lost stream.
pub fn run_pipe(args: &[String]) {
    use std::io::Read;
    let Some(fifo) = args.first() else {
        return;
    };
    // O_WRONLY on a FIFO blocks until a reader exists; fly opens its read end
    // (O_RDWR) before arming the pipe, so this returns at once in practice,
    // and an already-unlinked FIFO fails the open → silent exit.
    let Ok(mut out) = std::fs::OpenOptions::new().write(true).open(fifo) else {
        return;
    };
    // 64 KiB ≥ StdinLock's internal buffer, so each read is one read(2) on
    // fd 0 — no buffering layer sits between tmux and the FIFO.
    let mut buf = vec![0u8; 64 * 1024];
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if out.write_all(&buf[..n]).is_err() {
            return;
        }
    }
}
