//! `fly automation …` — the CLI surface over the automations store (U9,
//! R19–R24).
//!
//! Two transports, split by whether the op mutates:
//!
//! - **Read ops** (`list`, `show`, `runs`) read the store file directly
//!   ([`store::store_path`]), so they work **outside a pane** and even when the
//!   app isn't running (R19). Writes are atomic (temp + rename), so a
//!   concurrent read always sees a complete document.
//! - **Mutating ops** (`create`, `pause`, `resume`, `run`, `delete`) require the
//!   pane token and post over the hook socket (R19), where the app validates the
//!   token (the security boundary), enforces the R22 recursion gate, stamps
//!   origin, and writes a `{ok,…}` response. A slow/absent app is reported as
//!   "may have committed" rather than hanging (R20).
//!
//! The wire request/response types below are the shared contract with the
//! app-side handler in `lib.rs`.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::automations::model::{Automation, Mode, RunStatus};
use crate::automations::store;
use crate::notify;

/// Closed set of reasoning-effort levels accepted by `--effort`
/// (automations-workspace-and-model U2, R10). Mirrors Claude Code's own
/// `--effort` levels; validated CLI-side so a bad value is rejected before it
/// reaches the socket.
pub const VALID_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Validate the agent-only launch flags at `create` time (U2, R14). `--model`
/// and `--effort` require agent mode (rejected alongside `--script`), and
/// `--effort` must name a [`VALID_EFFORTS`] level (the model string itself is
/// opaque — aliases and full ids both pass through to `claude`). Returns the
/// message the CLI prints before exiting 2. Pure, so it is unit-tested without
/// the arg loop or the socket.
pub fn validate_agent_flags(
    script_present: bool,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), String> {
    if script_present && (model.is_some() || effort.is_some()) {
        return Err("--model / --effort are only valid with --prompt (agent mode)".to_string());
    }
    if let Some(level) = effort {
        if !VALID_EFFORTS.contains(&level) {
            return Err(format!(
                "--effort must be one of {} (got {level:?})",
                VALID_EFFORTS.join(", ")
            ));
        }
    }
    Ok(())
}

/// Request posted for an `automation/*` op (U9). The client fills `token`, `op`,
/// and the fields the op needs; the app-side handler in `lib.rs` deserializes
/// the same bytes and dispatches. All op-specific fields are optional so one
/// shape serves every op.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomationRequest {
    pub token: String,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Script **content** (read client-side from `--script`/`--script-file`);
    /// the app never opens a client path (R21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Agent-only: pin the launch model (alias or full id) and reasoning effort
    /// (automations-workspace-and-model U2, R9/R10/R14). Validated CLI-side
    /// (`effort` ∈ the closed set); the model string passes through opaque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Opt-in interrupt resilience (interrupt-resilience U4/R1): re-run once on
    /// the next launch a run this automation leaves interrupted by an app
    /// crash/restart. Create-only; `#[serde(default)]` = `false` for legacy
    /// clients and every other op.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_on_interrupt: bool,
}

/// The app's response to an `automation/*` request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomationResponse {
    pub ok: bool,
    /// Automation id (create) or run id (run) on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// R1 advisory min-gap warning (create) — surfaced, not fatal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AutomationResponse {
    pub fn ok(id: Option<String>, warning: Option<String>) -> Self {
        AutomationResponse {
            ok: true,
            id,
            warning,
            error: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        AutomationResponse {
            ok: false,
            id: None,
            warning: None,
            error: Some(msg.into()),
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        // A response must always serialize; fall back to a minimal error object.
        serde_json::to_vec(self)
            .unwrap_or_else(|_| br#"{"ok":false,"error":"response encode failed"}"#.to_vec())
    }
}

/// Dispatch `fly automation <sub> …`. `args` starts at the subcommand.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("create") => handle_create(&args[1..]),
        Some("list") => handle_list(&args[1..]),
        Some("show") => handle_show(&args[1..]),
        Some("runs") => handle_runs(&args[1..]),
        Some("pause") => handle_target("automation/pause", "paused", &args[1..]),
        Some("resume") => handle_target("automation/resume", "resumed", &args[1..]),
        Some("run") => handle_target("automation/run", "run started", &args[1..]),
        Some("delete") => handle_target("automation/delete", "deleted", &args[1..]),
        _ => {
            eprintln!(
                "usage: fly automation <create|list|show|runs|pause|resume|run|delete> …"
            );
            2
        }
    }
}

// ---- mutating ops (socket) --------------------------------------------------

/// Resolve the pane token + socket path from the environment (mutating ops only
/// work inside a fly pane, R19).
fn pane_env() -> Result<(String, String), String> {
    match (
        std::env::var("FLY_PANE_TOKEN"),
        std::env::var("FLY_SOCKET_PATH"),
    ) {
        (Ok(t), Ok(s)) if !t.is_empty() && !s.is_empty() => Ok((t, s)),
        _ => Err(
            "this command must run inside a fly pane (FLY_PANE_TOKEN unset)".to_string(),
        ),
    }
}

/// Post an automation request over the hook socket and read the response, with
/// a bounded wait (R20): a slow or absent app is reported as "may have
/// committed" rather than hanging.
pub fn send_request(
    socket: &Path,
    req: &AutomationRequest,
) -> Result<AutomationResponse, String> {
    let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot reach fly at {}: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    stream.write_all(&bytes).map_err(|e| e.to_string())?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    if (&stream).take(64 * 1024).read_to_end(&mut resp).is_err() || resp.is_empty() {
        return Err(
            "no response from fly within 5s — the change may have committed anyway".to_string(),
        );
    }
    serde_json::from_slice(&resp).map_err(|e| format!("bad response from fly: {e}"))
}

/// Send a request and print/exit uniformly. `--json` prints the raw response.
fn send_and_report(req: AutomationRequest, success_line: &str, json: bool) -> i32 {
    let (token, socket) = match pane_env() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fly automation: {e}");
            return 1;
        }
    };
    let req = AutomationRequest { token, ..req };
    match send_request(Path::new(&socket), &req) {
        Ok(resp) if resp.ok => {
            if json {
                println!("{}", serde_json::to_string(&resp).unwrap_or_default());
            } else {
                // R24: always print the banner on success (sanitized), even
                // when a store-flush warning rode along.
                let mut line = success_line.to_string();
                if let Some(id) = &resp.id {
                    line.push_str(&format!(" ({id})"));
                }
                println!("{line}");
                if let Some(w) = &resp.warning {
                    eprintln!("warning: {}", notify::sanitize_body(w));
                }
            }
            0
        }
        Ok(resp) => {
            let msg = resp.error.unwrap_or_else(|| "unknown error".to_string());
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&AutomationResponse::err(msg)).unwrap_or_default()
                );
            } else {
                eprintln!("fly automation: {}", notify::sanitize_body(&msg));
            }
            1
        }
        Err(e) => {
            eprintln!("fly automation: {e}");
            1
        }
    }
}

fn handle_create(args: &[String]) -> i32 {
    let mut name: Option<String> = None;
    let mut cron: Option<String> = None;
    let mut timezone: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut script: Option<String> = None;
    let mut script_file: Option<String> = None;
    let mut interpreter: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut retry_on_interrupt = false;
    let mut json = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("usage: fly automation create [options]");
                println!();
                println!("Options:");
                println!("  --name <name>           automation name (required)");
                println!("  --cron <expr>           cron schedule (required)");
                println!("  --tz, --timezone <tz>   timezone (default: UTC)");
                println!("  --cwd <path>            working directory (default: current)");
                println!("  --prompt <text>         agent mode: claude prompt");
                println!("  --model <name>          agent mode: pin the model (alias or full id)");
                println!("  --effort <level>        agent mode: reasoning effort (low, medium, high, xhigh, max)");
                println!("  --script <code>         script mode: inline script code");
                println!("  --script-file <path>    script mode: read script from file");
                println!("  --interpreter <name>    script interpreter: bash, sh, python3 (default: bash)");
                println!("  --timeout <ms>          script timeout in milliseconds (default: 120000)");
                println!("  --retry-on-interrupt    re-run once on the next launch if an app");
                println!("                          crash/restart interrupts a run (default: off)");
                println!("  --json                  output response as JSON");
                println!();
                println!("Either --prompt (agent mode) or --script/--script-file (script mode) is required.");
                println!("--model / --effort are agent-mode only.");
                println!();
                println!("Examples:");
                println!("  fly automation create --name 'check tests' --cron '*/5 * * * *' \\");
                println!("    --prompt 'run the test suite'");
                println!("  fly automation create --name 'backup db' --cron '0 2 * * *' \\");
                println!("    --script-file backup.sh");
                return 0;
            }
            "--name" => name = it.next().cloned(),
            "--cron" => cron = it.next().cloned(),
            "--tz" | "--timezone" => timezone = it.next().cloned(),
            "--cwd" => cwd = it.next().cloned(),
            "--prompt" => prompt = it.next().cloned(),
            "--model" => model = it.next().cloned(),
            "--effort" => effort = it.next().cloned(),
            "--script" => script = it.next().cloned(),
            "--script-file" => script_file = it.next().cloned(),
            "--retry-on-interrupt" => retry_on_interrupt = true,
            "--interpreter" => interpreter = it.next().cloned(),
            "--timeout" => {
                timeout_ms = match it.next().map(|s| s.parse::<u64>()) {
                    Some(Ok(n)) => Some(n),
                    _ => {
                        eprintln!("fly automation create: --timeout wants a millisecond integer");
                        return 2;
                    }
                }
            }
            "--json" => json = true,
            other => {
                eprintln!("fly automation create: unknown argument {other:?}");
                return 2;
            }
        }
    }

    let Some(name) = name else {
        eprintln!("fly automation create: --name is required");
        return 2;
    };
    let Some(cron) = cron else {
        eprintln!("fly automation create: --cron is required");
        return 2;
    };

    // Read a `--script-file` client-side — the app never opens a client path
    // (R21). `--script` (inline) and `--script-file` are mutually exclusive.
    if script.is_some() && script_file.is_some() {
        eprintln!("fly automation create: pass only one of --script / --script-file");
        return 2;
    }
    if let Some(path) = &script_file {
        match std::fs::read_to_string(path) {
            Ok(content) => script = Some(content),
            Err(e) => {
                eprintln!("fly automation create: cannot read --script-file {path:?}: {e}");
                return 1;
            }
        }
    }

    // Exactly one mode: agent (--prompt) XOR script (--script/--script-file).
    match (&prompt, &script) {
        (Some(_), Some(_)) => {
            eprintln!("fly automation create: pass either --prompt or --script, not both");
            return 2;
        }
        (None, None) => {
            eprintln!("fly automation create: one of --prompt or --script is required");
            return 2;
        }
        _ => {}
    }

    // A script run needs an interpreter from the closed enum; default bash.
    if script.is_some() {
        let name = interpreter.as_deref().unwrap_or("bash");
        if let Err(e) = crate::automations::script::resolve_interpreter(name) {
            eprintln!("fly automation create: {e}");
            return 2;
        }
        interpreter = Some(name.to_string());
    }

    // --model / --effort are agent-only and effort is a closed set (U2, R14).
    if let Err(e) = validate_agent_flags(script.is_some(), model.as_deref(), effort.as_deref()) {
        eprintln!("fly automation create: {e}");
        return 2;
    }

    // Default the cwd to where the CLI runs — i.e. the pane's cwd — so an
    // automation created here runs here (R1). Resolved client-side.
    let cwd = cwd.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });

    let req = AutomationRequest {
        op: "automation/create".to_string(),
        name: Some(name),
        cron: Some(cron),
        timezone: Some(timezone.unwrap_or_else(|| "UTC".to_string())),
        cwd,
        prompt,
        script,
        interpreter,
        timeout_ms,
        model,
        effort,
        retry_on_interrupt,
        ..Default::default()
    };
    send_and_report(req, "created", json)
}

/// pause / resume / run / delete: all take a single automation id.
fn handle_target(op: &str, success: &str, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        let cmd = match op {
            "automation/pause" => "pause",
            "automation/resume" => "resume",
            "automation/run" => "run",
            "automation/delete" => "delete",
            _ => "unknown",
        };
        println!("usage: fly automation {cmd} <id> [--json]");
        println!();
        match cmd {
            "pause" => println!("Pause a scheduled automation (it won't run until resumed)."),
            "resume" => println!("Resume a paused automation."),
            "run" => println!("Manually trigger an automation run immediately."),
            "delete" => println!("Delete an automation permanently."),
            _ => {}
        }
        return 0;
    }
    let mut id: Option<String> = None;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other if !other.starts_with('-') && id.is_none() => id = Some(other.to_string()),
            other => {
                eprintln!("fly automation: unexpected argument {other:?}");
                return 2;
            }
        }
    }
    let Some(id) = id else {
        eprintln!("fly automation: an automation id is required");
        return 2;
    };
    let req = AutomationRequest {
        op: op.to_string(),
        id: Some(id),
        ..Default::default()
    };
    send_and_report(req, success, json)
}

// ---- read ops (direct store read, R19) --------------------------------------

/// Load the store map directly from the default path (R19).
fn load_store() -> Result<Vec<Automation>, String> {
    load_store_at(&store::store_path())
}

/// Load + sort the store map directly from `path` (R19). A missing file (fresh
/// install) is an empty list; a corrupt/unreadable file is an error the caller
/// surfaces. Split out from [`load_store`] so the read path is testable without
/// touching the real XDG data dir.
pub fn load_store_at(path: &Path) -> Result<Vec<Automation>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let map: std::collections::BTreeMap<String, Automation> =
        serde_json::from_slice(&bytes).map_err(|e| format!("store file is corrupt: {e}"))?;
    let mut list: Vec<Automation> = map.into_values().collect();
    // Sort by next_run ascending, paused (None) last, then by name — the
    // dashboard's ordering (U10), applied here so the CLI list is stable.
    list.sort_by(|a, b| match (a.next_run_at, b.next_run_at) {
        (Some(x), Some(y)) => x.cmp(&y).then(a.name.cmp(&b.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    Ok(list)
}

fn wants_json(args: &[String]) -> bool {
    args.iter().any(|a| a == "--json")
}

fn handle_list(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: fly automation list [--json]");
        println!();
        println!("List all automations sorted by next run time.");
        return 0;
    }
    let json = wants_json(args);
    let list = match load_store() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fly automation list: {e}");
            return 1;
        }
    };
    if json {
        println!("{}", serde_json::to_string(&list).unwrap_or_default());
        return 0;
    }
    if list.is_empty() {
        println!("No automations. Run `fly automation create --help` to get started.");
        return 0;
    }
    let now = now_ms();
    for a in &list {
        println!(
            "{}  {}  [{}]  {} {}  next {}  last {}",
            a.id,
            notify::sanitize_title(&a.name),
            mode_label(&a.mode),
            a.cron,
            a.timezone,
            next_run_label(a.next_run_at, now),
            last_run_label(a),
        );
    }
    0
}

fn handle_show(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: fly automation show <id> [--json]");
        println!();
        println!("Show details of a single automation.");
        return 0;
    }
    let json = wants_json(args);
    let Some(id) = args.iter().find(|a| !a.starts_with('-')).cloned() else {
        eprintln!("fly automation show: an automation id is required");
        return 2;
    };
    let list = match load_store() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fly automation show: {e}");
            return 1;
        }
    };
    let Some(a) = list.into_iter().find(|a| a.id == id) else {
        eprintln!("fly automation show: no such automation: {id}");
        return 1;
    };
    if json {
        println!("{}", serde_json::to_string(&a).unwrap_or_default());
        return 0;
    }
    let now = now_ms();
    println!("id        {}", a.id);
    println!("name      {}", notify::sanitize_title(&a.name));
    println!("mode      {}", mode_label(&a.mode));
    println!("schedule  {} {}", a.cron, a.timezone);
    println!("cwd       {}", notify::sanitize_title(&a.cwd));
    println!("enabled   {}", a.enabled);
    println!("retry     {}", if a.retry_on_interrupt { "on interrupt" } else { "off" });
    println!("next run  {}", next_run_label(a.next_run_at, now));
    println!("last run  {}", last_run_label(&a));
    println!("runs      {} in history", a.runs.len());
    0
}

fn handle_runs(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: fly automation runs <id> [--output <run-id>] [--json]");
        println!();
        println!("List runs for an automation, or show output of a specific run.");
        println!();
        println!("Options:");
        println!("  --output <run-id>  print captured output from a single run");
        println!("  --json             output as JSON");
        return 0;
    }
    let json = wants_json(args);
    let mut id: Option<String> = None;
    let mut output_run: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => {}
            "--output" => output_run = it.next().cloned(),
            other if !other.starts_with('-') && id.is_none() => id = Some(other.to_string()),
            other => {
                eprintln!("fly automation runs: unexpected argument {other:?}");
                return 2;
            }
        }
    }
    let Some(id) = id else {
        eprintln!("fly automation runs: an automation id is required");
        return 2;
    };
    let list = match load_store() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fly automation runs: {e}");
            return 1;
        }
    };
    let Some(a) = list.into_iter().find(|a| a.id == id) else {
        eprintln!("fly automation runs: no such automation: {id}");
        return 1;
    };

    // `--output <runId>` prints just that run's captured output.
    if let Some(run_id) = output_run {
        let Some(row) = a.runs.iter().find(|r| r.id == run_id) else {
            eprintln!("fly automation runs: no such run: {run_id}");
            return 1;
        };
        match &row.output {
            Some(out) if json => {
                // --json emits the raw capture unmodified (machine path).
                println!("{}", serde_json::to_string(out).unwrap_or_default());
            }
            Some(out) => print!("{}", sanitize_output(out)),
            None if json => println!("null"),
            None => println!("(no output captured)"),
        }
        return 0;
    }

    if json {
        println!("{}", serde_json::to_string(&a.runs).unwrap_or_default());
        return 0;
    }
    if a.runs.is_empty() {
        println!("(no runs yet)");
        return 0;
    }
    let now = now_ms();
    for r in &a.runs {
        let when = r.started_at.or(r.finished_at);
        println!(
            "{}  {:<9}  {}  {}",
            r.id,
            status_label(r.status),
            when.map(|t| rel_label(t, now)).unwrap_or_else(|| "—".to_string()),
            r.error
                .as_deref()
                .map(notify::sanitize_title)
                .unwrap_or_default(),
        );
    }
    0
}

// ---- formatting helpers -----------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mode_label(mode: &Mode) -> &'static str {
    match mode {
        Mode::Agent { .. } => "agent",
        Mode::Script { .. } => "script",
    }
}

fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
    }
}

fn next_run_label(next: Option<u64>, now: u64) -> String {
    match next {
        None => "paused".to_string(),
        Some(t) => rel_label(t, now),
    }
}

fn last_run_label(a: &Automation) -> String {
    match a.last_run() {
        None => "never".to_string(),
        Some(r) => status_label(r.status).to_string(),
    }
}

/// A coarse relative time ("in 5m", "2h ago", "just now") — no formatting deps,
/// enough for a CLI glance; the dashboard (U10) does the richer humanization.
fn rel_label(target_ms: u64, now_ms: u64) -> String {
    let (past, delta) = if target_ms >= now_ms {
        (false, target_ms - now_ms)
    } else {
        (true, now_ms - target_ms)
    };
    let secs = delta / 1000;
    let body = if secs < 45 {
        return if past { "just now".into() } else { "in <1m".into() };
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    };
    if past {
        format!("{body} ago")
    } else {
        format!("in {body}")
    }
}

/// Strip terminal-dangerous control characters from captured output before
/// printing it to the user's terminal (R16/R20 — an untrusted script's stdout
/// must not inject escape sequences), while preserving newlines and tabs so a
/// multi-line capture stays readable (unlike [`notify::sanitize_body`], which
/// is for single-line notification text).
fn sanitize_output(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_label_covers_past_future_and_now() {
        let now = 1_000_000_000; // large enough that the past cases don't underflow
        assert_eq!(rel_label(now, now), "in <1m");
        assert_eq!(rel_label(now + 5 * 60_000, now), "in 5m");
        assert_eq!(rel_label(now + 2 * 3_600_000, now), "in 2h");
        assert_eq!(rel_label(now - 3 * 86_400_000, now), "3d ago");
        assert_eq!(rel_label(now - 10_000, now), "just now");
    }

    #[test]
    fn sanitize_output_strips_escapes_but_keeps_newlines() {
        let dirty = "line1\n\x1b[31mred\x07\nline2\t.";
        let clean = sanitize_output(dirty);
        // The ESC/BEL control bytes are stripped (so no terminal action fires),
        // while newlines and tabs survive so the capture stays readable. The
        // now-defanged escape's printable remainder ("[31m") is left as text.
        assert_eq!(clean, "line1\n[31mred\nline2\t.");
        assert!(!clean.contains('\x1b'), "escape byte stripped");
        assert!(!clean.contains('\x07'), "bell byte stripped");
    }

    #[test]
    fn load_store_at_reads_directly_and_sorts_paused_last() {
        // R19: read the store file directly (works outside a pane / with no app).
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("automations.json");
        assert!(
            load_store_at(&missing).unwrap().is_empty(),
            "a missing store is an empty list, not an error"
        );

        // Write a store via the real Store so the on-disk shape is authentic.
        let s = store::Store::load_at(missing.clone(), dir.path().join("scripts"));
        s.mutate(|map| {
            for (id, name, next) in [
                ("b", "beta", Some(3000u64)),
                ("a", "alpha", Some(1000)),
                ("p", "paused", None),
            ] {
                let mut a = super_automation(id, name);
                a.next_run_at = next;
                map.insert(id.to_string(), a);
            }
        })
        .unwrap();

        let list = load_store_at(&missing).unwrap();
        let order: Vec<&str> = list.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(order, ["a", "b", "p"], "next_run ascending, paused last");
    }

    /// A minimal stored automation for the read test.
    fn super_automation(id: &str, name: &str) -> Automation {
        Automation {
            id: id.into(),
            name: name.into(),
            cron: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            retry_on_interrupt: false,
            cwd: "/tmp".into(),
            mode: Mode::Script {
                script_file: "s".into(),
                interpreter: "bash".into(),
                timeout_ms: 120_000,
            },
            origin: crate::automations::model::Origin {
                pane_id: 1,
                workspace_id: "ws".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 0,
            next_run_at: Some(1000),
            runs: Vec::new(),
        }
    }

    #[test]
    fn response_roundtrips_and_encodes_ok_and_err() {
        let ok = AutomationResponse::ok(Some("a1".into()), Some("slow".into()));
        let back: AutomationResponse = serde_json::from_slice(&ok.to_bytes()).unwrap();
        assert!(back.ok);
        assert_eq!(back.id.as_deref(), Some("a1"));
        assert_eq!(back.warning.as_deref(), Some("slow"));

        let err = AutomationResponse::err("nope");
        let back: AutomationResponse = serde_json::from_slice(&err.to_bytes()).unwrap();
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("nope"));
    }

    // U2/R14: agent-mode model + a valid effort validate; no flags validate.
    #[test]
    fn validate_agent_flags_accepts_agent_model_effort_and_none() {
        assert!(validate_agent_flags(false, Some("opus"), Some("high")).is_ok());
        assert!(validate_agent_flags(false, None, None).is_ok());
        // Every closed-set effort level is accepted.
        for level in VALID_EFFORTS {
            assert!(
                validate_agent_flags(false, None, Some(level)).is_ok(),
                "effort {level:?} should validate"
            );
        }
    }

    // U2/R14: --model / --effort are agent-only — rejected with --script.
    #[test]
    fn validate_agent_flags_rejects_model_or_effort_in_script_mode() {
        let m = validate_agent_flags(true, Some("opus"), None).unwrap_err();
        assert!(m.contains("agent mode"), "model+script rejected: {m}");
        let e = validate_agent_flags(true, None, Some("high")).unwrap_err();
        assert!(e.contains("agent mode"), "effort+script rejected: {e}");
    }

    // U2/R10: an effort outside the closed set is rejected with the level named.
    #[test]
    fn validate_agent_flags_rejects_a_bogus_effort_level() {
        let m = validate_agent_flags(false, None, Some("bogus")).unwrap_err();
        assert!(m.contains("bogus"), "the bad level is echoed: {m}");
        assert!(m.contains("low"), "the valid set is listed: {m}");
    }
}
