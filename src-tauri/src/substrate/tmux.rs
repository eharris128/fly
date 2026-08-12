//! The tmux wrapper (U1): executor-seamed subprocess driver.
//!
//! Every tmux interaction in fly goes through [`Tmux`], which builds argument
//! vectors over an injected [`Executor`] so construction and error
//! classification are tested without a tmux binary. The production
//! [`RealExecutor`] runs `tmux` subprocesses with a hard wall-clock timeout —
//! a wedged server or fork-blocked host must never hang a caller
//! indefinitely (Gas City's `tmuxSubprocessTimeout` lesson).
//!
//! Server-lifecycle discipline (KTD3):
//! - fly starts the server itself (`start_server`) with a **scrubbed,
//!   explicit environment** — the server's global env is the inherited
//!   baseline of every future pane (the env-overlay trap), so the
//!   `CLAUDE_CODE_*` strip and the KTD12 substrate token are applied here,
//!   not per pane.
//! - `new_session` never lets tmux auto-start a server (that would inherit
//!   an unscrubbed env) and is preceded by the ga-h9z degraded-server
//!   preflight: a wedged-but-bound server must never be given the chance to
//!   unlink its socket and orphan every session.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::naming::validate_session_name;

/// Bogus, unrouteable target for the ga-h9z liveness probe (KTD3). A healthy
/// server answers "can't find session"; a dead one "no server running"; a
/// degraded one hangs or says something else.
const PROBE_SESSION: &str = "__fly_probe__";

/// Wall-clock cap for any single tmux subprocess (Gas City: 30 s production).
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Shorter cap for the pre-create liveness probe — a wedged server must fail
/// fast into [`TmuxError::Degraded`] rather than eat the full timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// `send-keys -l` size ceiling; longer payloads ride the paste-buffer leg
/// (KTD5; Gas City's `maxSendKeysLiteralLen`).
pub const SEND_KEYS_LITERAL_MAX: usize = 4096;

/// Classified tmux failures. Classification is by stderr *content* because
/// tmux's exit codes don't distinguish these; the strings are pinned by unit
/// tests so a tmux wording change fails loudly here instead of silently
/// misclassifying (their stability across 3.x is noted in the Gas City
/// mining).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxError {
    /// No server bound to the socket at all.
    NoServer,
    /// Server alive but holding zero sessions ("no current target").
    EmptyServer,
    /// ga-h9z: the socket exists but the server didn't answer the probe
    /// cleanly. Creating sessions now risks a socket clobber — refuse.
    Degraded,
    SessionNotFound(String),
    SessionExists(String),
    InvalidName(String),
    /// Subprocess-level failure: spawn error or wall-clock timeout.
    Io(String),
    /// tmux exited non-zero with an unclassified stderr.
    Failed { args: Vec<String>, stderr: String },
}

impl std::fmt::Display for TmuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TmuxError::NoServer => write!(f, "no tmux server running"),
            TmuxError::EmptyServer => write!(f, "tmux server holds no sessions"),
            TmuxError::Degraded => write!(
                f,
                "tmux server degraded: refusing to proceed to avoid socket clobber"
            ),
            TmuxError::SessionNotFound(s) => write!(f, "tmux session not found: {s}"),
            TmuxError::SessionExists(s) => write!(f, "tmux session already exists: {s}"),
            TmuxError::InvalidName(m) => write!(f, "{m}"),
            TmuxError::Io(m) => write!(f, "tmux subprocess error: {m}"),
            TmuxError::Failed { args, stderr } => {
                write!(f, "tmux {} failed: {}", args.join(" "), stderr.trim())
            }
        }
    }
}

/// A completed subprocess observation, pre-classification.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The seam: runs a tmux invocation. `env` (when `Some`) REPLACES the child
/// environment entirely — used by `start_server` to pin the scrubbed baseline;
/// `None` inherits fly's own env (fine for every post-start call: the server
/// env, not the client env, is what panes inherit).
pub trait Executor: Send + Sync {
    fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        env: Option<&BTreeMap<String, String>>,
        timeout: Duration,
    ) -> Result<ExecOutput, String>;
}

/// Production executor: real `tmux` subprocesses, hard timeout via a waiter
/// thread (std-only; no new crates).
pub struct RealExecutor;

impl Executor for RealExecutor {
    fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        env: Option<&BTreeMap<String, String>>,
        timeout: Duration,
    ) -> Result<ExecOutput, String> {
        let mut cmd = Command::new("tmux");
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = env {
            cmd.env_clear();
            cmd.envs(env);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawning tmux: {e}"))?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A paste payload is bounded (feed/peer caps) and far below pipe
            // capacity in the common case; a writer error only means the
            // child died first, which the wait below reports.
            let _ = pipe.write_all(bytes);
        }
        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::Builder::new()
            .name("fly-tmux-wait".into())
            .spawn(move || {
                let out = child.wait_with_output();
                let _ = tx.send(out);
            })
            .map_err(|e| format!("spawning waiter: {e}"))?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(out)) => {
                let _ = waiter.join();
                Ok(ExecOutput {
                    success: out.status.success(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                })
            }
            Ok(Err(e)) => Err(format!("waiting on tmux: {e}")),
            Err(_) => {
                // Timed out. The child may be wedged on the server socket; we
                // cannot SIGKILL through `wait_with_output` (it owns the
                // child), so the waiter thread is left to reap whenever the
                // process dies. Callers treat this as Io/Degraded.
                Err(format!("tmux timed out after {timeout:?}"))
            }
        }
    }
}

/// Wrapper configuration (per flavor).
#[derive(Debug, Clone)]
pub struct TmuxConfig {
    /// tmux `-L` socket name; derives from `FLY_APP_NAME` (R7 isolation).
    pub socket_name: String,
    /// `history-limit` pinned globally at server spawn (KTD9 — it binds
    /// panes at creation; setting it later misses existing sessions).
    pub history_limit: u32,
}

/// The utility session the persistent input client attaches to (Electron-
/// shell migration U6, the ~8 ms/keystroke fix). Deliberately OUTSIDE the
/// `fly-<flavor>-…` marked namespace, so discovery/adoption never sees it;
/// its only job is giving the control-mode client something to attach to
/// (control mode is "attach a client", and an attached marked session would
/// read as focused-elsewhere to the suppression policy).
const INPUT_CLIENT_SESSION: &str = "flyctl-input";

/// A persistent `tmux -C` control-mode client used as the interactive
/// keystroke transport: one long-lived process, one command line per
/// `send-keys`, ~0.003 ms caller cost vs ~8 ms for a subprocess per
/// keystroke (measured 2026-08-12, this box). Fire-and-forget: responses
/// are drained (a full pipe would wedge the server's writes), errors are
/// not read back — a keystroke to a dead session drops, exactly like a PTY
/// write to a dead child; session death is owned by the pane-died hook +
/// poll pipeline, not the input path. Any write failure drops the client;
/// the caller falls back to the subprocess leg and the next call respawns.
struct InputClient {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
}

impl InputClient {
    fn spawn(socket_name: &str) -> Option<InputClient> {
        let mut child = Command::new("tmux")
            .args([
                "-u",
                "-L",
                socket_name,
                "-C",
                // NO `-d`: control mode is "attach a client" — with -d the
                // client exits the moment the (detached) session exists, and
                // a write can race into the dying pipe and silently drop a
                // keystroke (caught by substrate_live on first landing).
                "new-session",
                "-A",
                "-s",
                INPUT_CLIENT_SESSION,
                "sleep infinity",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        // Drain everything the server sends (%begin/%end blocks, %output
        // notifications) so the pipe never fills.
        std::thread::Builder::new()
            .name("fly-tmux-input-drain".into())
            .spawn(move || {
                use std::io::Read as _;
                let mut sink = [0u8; 4096];
                let mut r = stdout;
                while matches!(r.read(&mut sink), Ok(n) if n > 0) {}
            })
            .ok()?;
        Some(InputClient { child, stdin })
    }

    fn send_line(&mut self, line: &str) -> bool {
        self.stdin.write_all(line.as_bytes()).is_ok() && self.stdin.flush().is_ok()
    }
}

impl Drop for InputClient {
    fn drop(&mut self) {
        // A control client is a plain child process: kill + reap, or it
        // would outlive fly attached to the server forever.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The tmux driver. Construction is cheap; methods classify every failure.
pub struct Tmux {
    cfg: TmuxConfig,
    exec: Box<dyn Executor>,
    /// Lazily-spawned persistent input client (`None` until first use or
    /// after a failure). Disabled under `with_executor` so unit tests with
    /// fake executors observe every send as subprocess args.
    input: std::sync::Mutex<Option<InputClient>>,
    input_enabled: bool,
}

impl Tmux {
    pub fn new(cfg: TmuxConfig) -> Self {
        Self {
            cfg,
            exec: Box::new(RealExecutor),
            input: std::sync::Mutex::new(None),
            input_enabled: true,
        }
    }

    pub fn with_executor(cfg: TmuxConfig, exec: Box<dyn Executor>) -> Self {
        Self {
            cfg,
            exec,
            input: std::sync::Mutex::new(None),
            input_enabled: false,
        }
    }

    /// Base argv: `-u` always (UTF-8 regardless of locale), then the flavor
    /// socket.
    fn base(&self) -> Vec<String> {
        vec!["-u".into(), "-L".into(), self.cfg.socket_name.clone()]
    }

    fn run(&self, tail: &[&str]) -> Result<ExecOutput, TmuxError> {
        self.run_full(tail, None, None, SUBPROCESS_TIMEOUT)
    }

    fn run_full(
        &self,
        tail: &[&str],
        stdin: Option<&[u8]>,
        env: Option<&BTreeMap<String, String>>,
        timeout: Duration,
    ) -> Result<ExecOutput, TmuxError> {
        let mut args = self.base();
        args.extend(tail.iter().map(|s| s.to_string()));
        self.exec
            .run(&args, stdin, env, timeout)
            .map_err(TmuxError::Io)
    }

    /// Classify a non-success output against the pinned stderr taxonomy.
    fn classify(args: &[&str], out: &ExecOutput) -> TmuxError {
        let stderr = out.stderr.trim();
        if stderr.contains("no current target") {
            return TmuxError::EmptyServer;
        }
        if stderr.contains("no server running") || stderr.contains("error connecting to") {
            return TmuxError::NoServer;
        }
        if stderr.contains("can't find session") || stderr.contains("session not found") {
            return TmuxError::SessionNotFound(stderr.to_string());
        }
        if stderr.contains("duplicate session") {
            return TmuxError::SessionExists(stderr.to_string());
        }
        TmuxError::Failed {
            args: args.iter().map(|s| s.to_string()).collect(),
            stderr: stderr.to_string(),
        }
    }

    /// tmux version as (major, minor), tolerant of suffixes ("3.4a", "next-3.6").
    pub fn version(&self) -> Result<(u32, u32), TmuxError> {
        let out = self.run_full(&["-V"], None, None, PROBE_TIMEOUT)?;
        if !out.success {
            return Err(Self::classify(&["-V"], &out));
        }
        parse_version(&out.stdout).ok_or_else(|| TmuxError::Io(format!(
            "unparseable tmux version {:?}",
            out.stdout.trim()
        )))
    }

    /// R8 floor: `-e` on new-session needs ≥ 3.2 (validated on 3.4).
    pub fn meets_version_floor(&self) -> Result<bool, TmuxError> {
        let (maj, min) = self.version()?;
        Ok(maj > 3 || (maj == 3 && min >= 2))
    }

    /// ga-h9z preflight (KTD3): is the server bound to our socket healthy?
    /// `Ok(true)` = alive, `Ok(false)` = cleanly absent (safe to start one),
    /// `Err(Degraded)` = bound but wrong/absent answer — do NOT create.
    pub fn probe_server_alive(&self) -> Result<bool, TmuxError> {
        let args = ["has-session", "-t", PROBE_SESSION];
        match self.run_full(&args, None, None, PROBE_TIMEOUT) {
            Ok(out) if out.success => {
                // A real `__fly_probe__` session should be impossible; treat
                // an affirmative answer as alive anyway.
                Ok(true)
            }
            Ok(out) => match Self::classify(&args, &out) {
                // Healthy negatives: "can't find session" (server holds
                // sessions) or "no current target" (server alive, ZERO
                // sessions — the Gas City ErrNoCurrentTarget case, hit live
                // right after `start_server`).
                TmuxError::SessionNotFound(_) | TmuxError::EmptyServer => Ok(true),
                TmuxError::NoServer => Ok(false), // clean absence
                _ => Err(TmuxError::Degraded),
            },
            Err(TmuxError::Io(_)) => Err(TmuxError::Degraded), // hung probe
            Err(e) => Err(e),
        }
    }

    /// Start the per-flavor server with a **fully explicit environment**
    /// (KTD3/KTD8/KTD12): the caller passes the scrubbed baseline including
    /// `FLY_SUBSTRATE_TOKEN` and the stable `FLY_SOCKET_PATH`. Then pins the
    /// server options fly's lifecycle depends on: `exit-empty off` (the
    /// server and its socket must survive zero sessions) and the global
    /// `history-limit` (KTD9).
    pub fn start_server(&self, env: &BTreeMap<String, String>) -> Result<(), TmuxError> {
        // One invocation, `;`-chained: a bare `start-server` exits the moment
        // the command completes (default `exit-empty on`) — the options must
        // land in the same client connection that started it, and
        // `exit-empty off` is what then holds the empty server alive.
        let limit = self.cfg.history_limit.to_string();
        let args = [
            "start-server",
            ";",
            "set-option",
            "-g",
            "exit-empty",
            "off",
            ";",
            "set-option",
            "-g",
            "history-limit",
            &limit,
        ];
        let out = self.run_full(&args, None, Some(env), SUBPROCESS_TIMEOUT)?;
        if !out.success {
            return Err(Self::classify(&args, &out));
        }
        Ok(())
    }

    /// Kill the flavor server outright (scratch/test teardown; production
    /// quit detaches instead — KTD7).
    pub fn kill_server(&self) -> Result<(), TmuxError> {
        let out = self.run(&["kill-server"])?;
        if out.success {
            return Ok(());
        }
        match Self::classify(&["kill-server"], &out) {
            TmuxError::NoServer | TmuxError::EmptyServer => Ok(()),
            e => Err(e),
        }
    }

    pub fn has_session(&self, name: &str) -> Result<bool, TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        let args = ["has-session", "-t", name];
        let out = self.run(&args)?;
        if out.success {
            return Ok(true);
        }
        match Self::classify(&args, &out) {
            TmuxError::SessionNotFound(_) | TmuxError::NoServer | TmuxError::EmptyServer => {
                Ok(false)
            }
            e => Err(e),
        }
    }

    /// Create a detached, marked session (KTD3/KTD4): validated name, `-c`
    /// cwd, sorted `-e` env, the pane command as the initial process (no
    /// shell-ready race), window geometry pinned `manual` + sized to fly's
    /// grid (KTD2 — tmux ≥3.3 would otherwise lock detached sessions at
    /// 80×24). The ga-h9z preflight runs first and a missing server is an
    /// error, never an implicit `tmux` auto-start (which would inherit an
    /// unscrubbed client env).
    #[allow(clippy::too_many_arguments)]
    pub fn new_session(
        &self,
        name: &str,
        cwd: &str,
        env: &BTreeMap<String, String>,
        command: &[String],
        cols: u16,
        rows: u16,
    ) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        if !self.probe_server_alive()? {
            return Err(TmuxError::NoServer);
        }
        let mut args: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name.into(),
            "-x".into(),
            cols.to_string(),
            "-y".into(),
            rows.to_string(),
        ];
        if !cwd.is_empty() {
            args.push("-c".into());
            args.push(cwd.into());
        }
        for (k, v) in env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.extend(command.iter().cloned());
        let strs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&strs)?;
        if !out.success {
            return Err(Self::classify(&strs, &out));
        }
        // KTD2: detached geometry is fly-driven.
        self.set_window_size_manual(name)?;
        Ok(())
    }

    pub fn kill_session(&self, name: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        let args = ["kill-session", "-t", name];
        let out = self.run(&args)?;
        if out.success {
            return Ok(());
        }
        match Self::classify(&args, &out) {
            // Idempotent teardown: already gone is success.
            TmuxError::SessionNotFound(_) | TmuxError::NoServer | TmuxError::EmptyServer => Ok(()),
            e => Err(e),
        }
    }

    /// Names of sessions carrying fly's `fly-<flavor>-` mark (KTD4). An
    /// empty or absent server is an empty list, not an error.
    pub fn list_marked_sessions(&self) -> Result<Vec<String>, TmuxError> {
        let prefix = format!("fly-{}-", self.flavor_from_socket());
        let args = ["list-sessions", "-F", "#{session_name}"];
        let out = self.run(&args)?;
        if !out.success {
            return match Self::classify(&args, &out) {
                TmuxError::NoServer | TmuxError::EmptyServer => Ok(vec![]),
                e => Err(e),
            };
        }
        Ok(out
            .stdout
            .lines()
            .filter(|l| l.starts_with(&prefix))
            .map(|l| l.to_string())
            .collect())
    }

    /// The flavor is encoded in the socket name (`fly` / `fly-dev`); the
    /// session mark reuses it so two flavors' marks can never collide even
    /// if they somehow shared a server.
    fn flavor_from_socket(&self) -> &str {
        &self.cfg.socket_name
    }

    /// KTD2 detached mode: manual sizing, fly drives the grid.
    pub fn set_window_size_manual(&self, name: &str) -> Result<(), TmuxError> {
        self.simple(&["set-option", "-wt", name, "window-size", "manual"])
    }

    /// KTD2 attached mode: the human's client wins.
    pub fn set_window_size_latest(&self, name: &str) -> Result<(), TmuxError> {
        self.simple(&["set-option", "-wt", name, "window-size", "latest"])
    }

    /// Drive a detached session's grid to fly's pane size (KTD2).
    pub fn resize_window(&self, name: &str, cols: u16, rows: u16) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        self.simple(&[
            "resize-window",
            "-t",
            name,
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ])
    }

    /// Rendered grid text; `escapes` includes SGR (`-e`) for the mirror /
    /// screen-parse paths (KTD9). `history_lines` reaches back into
    /// scrollback (`-S -N`); 0 captures the visible screen only.
    pub fn capture_pane(
        &self,
        name: &str,
        escapes: bool,
        history_lines: u32,
    ) -> Result<String, TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        let start = format!("-{history_lines}");
        let mut args = vec!["capture-pane", "-p", "-t", name];
        if escapes {
            args.push("-e");
        }
        if history_lines > 0 {
            args.push("-S");
            args.push(&start);
        }
        let out = self.run(&args)?;
        if !out.success {
            return Err(Self::classify(&args, &out));
        }
        Ok(out.stdout)
    }

    /// KTD5 delivery ladder, transport half: literal text via `send-keys -l`
    /// under the ceiling, else a stdin-fed `load-buffer` + forced bracketed
    /// `paste-buffer -p -d`. Submit confirmation (the other half of KTD5)
    /// lives with the delivery routes (U6), not here.
    pub fn send_text(&self, name: &str, text: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        if text.len() <= SEND_KEYS_LITERAL_MAX {
            return self.simple(&["send-keys", "-t", name, "-l", text]);
        }
        let buf = format!("fly-paste-{}", std::process::id());
        let load = ["load-buffer", "-b", &buf, "-"];
        let out = self.run_full(&load, Some(text.as_bytes()), None, SUBPROCESS_TIMEOUT)?;
        if !out.success {
            return Err(Self::classify(&load, &out));
        }
        self.simple(&["paste-buffer", "-p", "-d", "-b", &buf, "-t", name])
    }

    /// A named tmux key ("Enter", "Escape", "C-c").
    pub fn send_key(&self, name: &str, key: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        self.simple(&["send-keys", "-t", name, key])
    }

    /// Binary-safe raw input: `send-keys -H` hex bytes (tmux ≥ 2.4). The
    /// interactive keystroke transport for tmux-backed panes (U3) —
    /// arbitrary control sequences pass byte-exact, no `-l` quoting rules.
    ///
    /// Rides the persistent control-mode client when available (U6: ~8 ms
    /// subprocess exec per keystroke → ~µs pipe write; see [`InputClient`]
    /// for the fire-and-forget tradeoff), falling back to — and respawning
    /// through — the one-subprocess leg on any client failure.
    pub fn send_hex(&self, name: &str, bytes: &[u8]) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        if self.input_enabled && self.send_hex_via_client(name, bytes) {
            return Ok(());
        }
        let mut args: Vec<String> =
            vec!["send-keys".into(), "-t".into(), name.into(), "-H".into()];
        args.extend(bytes.iter().map(|b| format!("{b:02x}")));
        let strs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&strs)?;
        if !out.success {
            return Err(Self::classify(&strs, &out));
        }
        Ok(())
    }

    /// Fast-path attempt: write one `send-keys` command line to the
    /// persistent client, (re)spawning it lazily. `false` = caller must use
    /// the subprocess leg (spawn failed or the client died mid-write; the
    /// dead client is dropped so the next call respawns fresh).
    fn send_hex_via_client(&self, name: &str, bytes: &[u8]) -> bool {
        let mut line = String::with_capacity(24 + name.len() + bytes.len() * 3);
        line.push_str("send-keys -t ");
        line.push_str(name);
        line.push_str(" -H");
        for b in bytes {
            line.push_str(&format!(" {b:02x}"));
        }
        line.push('\n');
        let mut guard = match self.input.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.is_none() {
            *guard = InputClient::spawn(&self.cfg.socket_name);
        }
        match guard.as_mut() {
            Some(client) => {
                if client.send_line(&line) {
                    true
                } else {
                    *guard = None; // dead client: drop (kills + reaps), respawn next call
                    false
                }
            }
            None => false,
        }
    }

    /// `remain-on-exit` for a session's window (U4/KTD4): a dead pane keeps
    /// its final screen instead of taking the session down.
    pub fn set_remain_on_exit(&self, name: &str, on: bool) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        let v = if on { "on" } else { "off" };
        self.simple(&["set-option", "-wt", name, "remain-on-exit", v])
    }

    /// Whether the session's pane process has died (`Some(exit_status)`) or
    /// is alive (`None`). Requires `remain-on-exit on` for the dead pane to
    /// still be observable.
    pub fn pane_dead(&self, name: &str) -> Result<Option<i32>, TmuxError> {
        let out = self.display_message(name, "#{pane_dead} #{pane_dead_status}")?;
        let mut parts = out.split_whitespace();
        match parts.next() {
            Some("1") => Ok(Some(parts.next().and_then(|s| s.parse().ok()).unwrap_or(0))),
            _ => Ok(None),
        }
    }

    /// One snapshot of every marked session's death state (U4's poll
    /// backstop): a single `list-panes -a` subprocess, returning
    /// `(session, exit_status)` for marked sessions whose pane process has
    /// died. Absent/empty server ⇒ empty.
    pub fn list_dead_marked(&self) -> Result<Vec<(String, i32)>, TmuxError> {
        let prefix = format!("fly-{}-", self.flavor_from_socket());
        let args = [
            "list-panes",
            "-a",
            "-F",
            "#{session_name} #{pane_dead} #{pane_dead_status}",
        ];
        let out = self.run(&args)?;
        if !out.success {
            return match Self::classify(&args, &out) {
                TmuxError::NoServer | TmuxError::EmptyServer => Ok(vec![]),
                e => Err(e),
            };
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| {
                let mut parts = l.split_whitespace();
                let name = parts.next()?;
                let dead = parts.next()?;
                let status: i32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                (dead == "1" && name.starts_with(&prefix))
                    .then(|| (name.to_string(), status))
            })
            .collect())
    }

    /// One-shot format expansion against a session's active pane.
    pub fn display_message(&self, name: &str, format: &str) -> Result<String, TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        let args = ["display-message", "-t", name, "-p", format];
        let out = self.run(&args)?;
        if !out.success {
            return Err(Self::classify(&args, &out));
        }
        Ok(out.stdout)
    }

    /// SIGWINCH wake for detached TUIs (KTD5 insurance): resize down-up.
    pub fn wake_pane(&self, name: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        self.simple(&["resize-pane", "-t", name, "-y", "-1"])?;
        std::thread::sleep(Duration::from_millis(50));
        self.simple(&["resize-pane", "-t", name, "-y", "+1"])
    }

    pub fn is_session_attached(&self, name: &str) -> Result<bool, TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        let args = ["display-message", "-t", name, "-p", "#{session_attached}"];
        let out = self.run(&args)?;
        if !out.success {
            return Err(Self::classify(&args, &out));
        }
        Ok(out.stdout.trim() != "0" && !out.stdout.trim().is_empty())
    }

    /// Arm the pane-death report (KTD1/KTD12): `remain-on-exit on` so the
    /// final screen survives (KTD4 — never auto-killed), then a `pane-died`
    /// hook running the fly CLI event op. The hook executes in the server's
    /// context: its auth is the server-env `FLY_SUBSTRATE_TOKEN`, and the
    /// only interpolations are the validated session name and tmux's own
    /// `#{pane_dead_status}` (KTD11).
    pub fn arm_pane_died_hook(&self, name: &str, fly_bin: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        if fly_bin.contains('\'') {
            return Err(TmuxError::InvalidName(format!(
                "fly binary path {fly_bin:?} contains a quote"
            )));
        }
        self.simple(&["set-option", "-t", name, "remain-on-exit", "on"])?;
        let hook = format!(
            "run-shell \"'{fly_bin}' substrate-event pane-died '{name}' '#{{pane_dead_status}}'\""
        );
        self.simple(&["set-hook", "-t", name, "pane-died", &hook])
    }

    /// Arm attach-state reports (KTD1/KTD6/KTD12).
    pub fn arm_attach_hooks(&self, name: &str, fly_bin: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        if fly_bin.contains('\'') {
            return Err(TmuxError::InvalidName(format!(
                "fly binary path {fly_bin:?} contains a quote"
            )));
        }
        for (event, arg) in [("client-attached", "attached"), ("client-detached", "detached")] {
            let hook = format!(
                "run-shell \"'{fly_bin}' substrate-event attach-state '{name}' '{arg}'\""
            );
            self.simple(&["set-hook", "-t", name, event, &hook])?;
        }
        Ok(())
    }

    /// Open the focused pane's byte stream into a FIFO (U3). `-o` opens only
    /// when no pipe exists, so restart re-arming is idempotent.
    pub fn pipe_pane_open(&self, name: &str, fifo: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        if fifo.contains('\'') {
            return Err(TmuxError::InvalidName(format!(
                "fifo path {fifo:?} contains a quote"
            )));
        }
        let cmd = format!("cat > '{fifo}'");
        self.simple(&["pipe-pane", "-o", "-t", name, &cmd])
    }

    /// Close a pane's pipe (bare `pipe-pane`).
    pub fn pipe_pane_close(&self, name: &str) -> Result<(), TmuxError> {
        validate_session_name(name).map_err(TmuxError::InvalidName)?;
        self.simple(&["pipe-pane", "-t", name])
    }

    fn simple(&self, args: &[&str]) -> Result<(), TmuxError> {
        let out = self.run(args)?;
        if !out.success {
            return Err(Self::classify(args, &out));
        }
        Ok(())
    }
}

/// Parse `tmux 3.4` / `tmux 3.4a` / `tmux next-3.6` into (major, minor).
fn parse_version(s: &str) -> Option<(u32, u32)> {
    let tail = s.trim().rsplit(' ').next()?;
    let tail = tail.strip_prefix("next-").unwrap_or(tail);
    let mut parts = tail.splitn(2, '.');
    let maj: u32 = parts.next()?.parse().ok()?;
    let min_raw = parts.next()?;
    let digits: String = min_raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().map(|m| (maj, m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted fake: records invocations, pops queued outputs (or succeeds
    /// silently when the queue is empty).
    struct FakeExec {
        calls: Mutex<Vec<(Vec<String>, Option<Vec<u8>>)>>,
        script: Mutex<Vec<Result<ExecOutput, String>>>,
    }

    impl FakeExec {
        fn new(script: Vec<Result<ExecOutput, String>>) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                script: Mutex::new(script),
            }
        }
        fn ok() -> Result<ExecOutput, String> {
            Ok(ExecOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn fail(stderr: &str) -> Result<ExecOutput, String> {
            Ok(ExecOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.into(),
            })
        }
    }

    impl Executor for &FakeExec {
        fn run(
            &self,
            args: &[String],
            stdin: Option<&[u8]>,
            _env: Option<&BTreeMap<String, String>>,
            _timeout: Duration,
        ) -> Result<ExecOutput, String> {
            self.calls
                .lock()
                .unwrap()
                .push((args.to_vec(), stdin.map(|b| b.to_vec())));
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                FakeExec::ok()
            } else {
                script.remove(0)
            }
        }
    }

    fn tmux(fake: &'static FakeExec) -> Tmux {
        Tmux::with_executor(
            TmuxConfig {
                socket_name: "fly".into(),
                history_limit: 10_000,
            },
            Box::new(fake),
        )
    }

    fn leak(f: FakeExec) -> &'static FakeExec {
        Box::leak(Box::new(f))
    }

    #[test]
    fn new_session_arg_construction_sorted_env_and_geometry() {
        let fake = leak(FakeExec::new(vec![
            FakeExec::fail("can't find session: __fly_probe__"), // probe → alive
        ]));
        let t = tmux(fake);
        let mut env = BTreeMap::new();
        env.insert("FLY_PANE_TOKEN".to_string(), "tok".to_string());
        env.insert("B_VAR".to_string(), "2".to_string());
        t.new_session(
            "fly-fly-leaf1",
            "/home/x",
            &env,
            &["claude".to_string()],
            120,
            40,
        )
        .unwrap();
        let calls = fake.calls.lock().unwrap();
        // probe, new-session, window-size manual
        assert_eq!(calls.len(), 3);
        let ns = &calls[1].0;
        let joined = ns.join(" ");
        assert!(joined.starts_with("-u -L fly new-session -d -s fly-fly-leaf1 -x 120 -y 40"));
        assert!(joined.contains("-c /home/x"));
        // BTreeMap ⇒ sorted: B_VAR before FLY_PANE_TOKEN.
        let b = joined.find("B_VAR=2").unwrap();
        let f = joined.find("FLY_PANE_TOKEN=tok").unwrap();
        assert!(b < f);
        assert!(joined.ends_with("claude"));
        assert_eq!(calls[2].0.join(" "), "-u -L fly set-option -wt fly-fly-leaf1 window-size manual");
    }

    #[test]
    fn new_session_refuses_missing_server_rather_than_autostart() {
        let fake = leak(FakeExec::new(vec![FakeExec::fail("no server running on /run/x")]));
        let t = tmux(fake);
        let err = t
            .new_session("fly-fly-a", "", &BTreeMap::new(), &[], 80, 24)
            .unwrap_err();
        assert_eq!(err, TmuxError::NoServer);
        assert_eq!(fake.calls.lock().unwrap().len(), 1); // probe only
    }

    #[test]
    fn probe_classifies_healthy_dead_and_degraded() {
        let healthy = leak(FakeExec::new(vec![FakeExec::fail("can't find session")]));
        assert_eq!(tmux(healthy).probe_server_alive().unwrap(), true);

        // A just-started empty server answers "no current target" — alive
        // (hit live 2026-08-11; the Gas City ErrNoCurrentTarget case).
        let empty = leak(FakeExec::new(vec![FakeExec::fail("no current target")]));
        assert_eq!(tmux(empty).probe_server_alive().unwrap(), true);

        let dead = leak(FakeExec::new(vec![FakeExec::fail("error connecting to /run/y")]));
        assert_eq!(tmux(dead).probe_server_alive().unwrap(), false);

        let odd = leak(FakeExec::new(vec![FakeExec::fail("protocol version mismatch")]));
        assert_eq!(tmux(odd).probe_server_alive().unwrap_err(), TmuxError::Degraded);

        let hung = leak(FakeExec::new(vec![Err("tmux timed out after 2s".into())]));
        assert_eq!(tmux(hung).probe_server_alive().unwrap_err(), TmuxError::Degraded);
    }

    #[test]
    fn send_text_routes_by_size() {
        let fake = leak(FakeExec::new(vec![]));
        let t = tmux(fake);
        t.send_text("fly-fly-a", "short").unwrap();
        let long = "x".repeat(SEND_KEYS_LITERAL_MAX + 1);
        t.send_text("fly-fly-a", &long).unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(calls[0].0.join(" ").contains("send-keys -t fly-fly-a -l short"));
        assert!(calls[1].0.join(" ").contains("load-buffer"));
        assert_eq!(calls[1].1.as_ref().unwrap().len(), long.len()); // stdin-fed
        let paste = calls[2].0.join(" ");
        assert!(paste.contains("paste-buffer -p -d -b"));
    }

    #[test]
    fn kill_session_is_idempotent_across_gone_states() {
        for stderr in ["can't find session: x", "no server running", "no current target"] {
            let fake = leak(FakeExec::new(vec![FakeExec::fail(stderr)]));
            tmux(fake).kill_session("fly-fly-a").unwrap();
        }
    }

    #[test]
    fn list_marked_filters_by_flavor_prefix_and_tolerates_empty() {
        let fake = leak(FakeExec::new(vec![Ok(ExecOutput {
            success: true,
            stdout: "fly-fly-a\nuser-session\nfly-fly-dev-b\nfly-fly-c\n".into(),
            stderr: String::new(),
        })]));
        let got = tmux(fake).list_marked_sessions().unwrap();
        assert_eq!(got, vec!["fly-fly-a", "fly-fly-dev-b", "fly-fly-c"]);
        // NOTE: "fly-fly-dev-b" matches the "fly-fly-" prefix — flavor
        // prefixes are themselves prefix-free in practice ("fly" vs
        // "fly-dev" produce marks "fly-fly-" vs "fly-fly-dev-"), which
        // overlap. Recorded as a naming refinement for U3 (see plan).

        let empty = leak(FakeExec::new(vec![FakeExec::fail("no server running")]));
        assert_eq!(tmux(empty).list_marked_sessions().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn hooks_reject_quoted_paths_and_embed_validated_name() {
        let fake = leak(FakeExec::new(vec![]));
        let t = tmux(fake);
        t.arm_pane_died_hook("fly-fly-a", "/usr/bin/fly").unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(calls[0].0.join(" ").contains("remain-on-exit on"));
        let hook = calls[1].0.join(" ");
        assert!(hook.contains("pane-died"));
        assert!(hook.contains("substrate-event pane-died 'fly-fly-a' '#{pane_dead_status}'"));
        drop(calls);
        assert!(t.arm_pane_died_hook("fly-fly-a", "/tmp/it's").is_err());
    }

    #[test]
    fn invalid_names_refused_before_any_subprocess() {
        let fake = leak(FakeExec::new(vec![]));
        let t = tmux(fake);
        assert!(matches!(
            t.send_text("bad.name", "x").unwrap_err(),
            TmuxError::InvalidName(_)
        ));
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn version_parse_tolerates_suffixes() {
        assert_eq!(parse_version("tmux 3.4"), Some((3, 4)));
        assert_eq!(parse_version("tmux 3.4a"), Some((3, 4)));
        assert_eq!(parse_version("tmux next-3.6"), Some((3, 6)));
        assert_eq!(parse_version("garbage"), None);
    }
}
