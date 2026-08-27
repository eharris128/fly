//! `fly automation …` — the CLI surface over the automations store (U9,
//! R19–R24).
//!
//! Two transports, split by whether the op mutates:
//!
//! - **Read ops** (`list`, `show`, `runs`) read the store file directly
//!   ([`store::store_path`]), so they work **outside a pane** and even when the
//!   app isn't running (R19). Writes are atomic (temp + rename), so a
//!   concurrent read always sees a complete document.
//! - **Mutating ops** (`create`, `update`, `pause`, `resume`, `run`, `delete`)
//!   require the
//!   pane token and post over the hook socket (R19), where the app validates the
//!   token (the security boundary), enforces the R22 recursion gate, stamps
//!   origin, and writes a `{ok,…}` response. A slow/absent app is reported as
//!   "may have committed" rather than hanging (R20).
//!
//! The wire request/response types below are the shared contract with the
//! app-side handler in `lib.rs`.
//!
//! Monitor surface (U5 of
//! `docs/plans/2026-07-10-002-feat-monitor-handoff-plan.md` — monitor-handoff
//! IDs below say so explicitly): `create` grows `--monitor` + `--not-before`
//! (monitor-handoff R1), the R8 Sonnet-at-xhigh launch default is stamped
//! here at create time ([`monitor_launch_defaults`]), and `list`/`show`/
//! `runs` render the monitor states (parked / retired pass / retired fail /
//! broken / paused — monitor-handoff R18's CLI mirror). This is the surface
//! the U8 skill drives (monitor-handoff R10).
//!
//! Update surface (U4 of
//! `docs/plans/2026-08-08-001-feat-automation-update-plan.md`): `update`
//! patches a stored record in place with **patch semantics** — absent flag =
//! unchanged, `--no-*` flags clear a pin through the additive `clear` list
//! (KTD1) — keeping the id, run history, origin and any dependency edge that
//! delete + recreate would lose. What it deliberately cannot do (KTD2) has no
//! flag at all: mode-kind switches, monitors, retired records, `cwd`, and
//! *setting* `--after`.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::automations::model::{Automation, Mode, RunRow, RunStatus, Verdict, VerdictOutcome};
use crate::automations::store;
use crate::automations::verdict::MONITOR_BROKEN_THRESHOLD;
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

/// Monitor-handoff R8: the launch model a monitor's checks default to when
/// `--model` is unspecified.
pub const MONITOR_DEFAULT_MODEL: &str = "sonnet";
/// Monitor-handoff R8: the reasoning effort a monitor's checks default to
/// when `--effort` is unspecified (∈ [`VALID_EFFORTS`]).
pub const MONITOR_DEFAULT_EFFORT: &str = "xhigh";

/// Validate the monitor flags at `create` time (monitor-handoff U5, R1).
/// `--monitor` is agent-mode only, like `--model`/`--effort` (the socket
/// enforces the same rule on the untrusted wire — U4); `--not-before` is
/// monitor-only — the plan keeps the floor a monitor concept, so an orphan
/// `--not-before` is rejected rather than silently riding an ordinary
/// recurring automation. Pure; returns the message the CLI prints before
/// exiting 2 (the [`validate_agent_flags`] shape).
pub fn validate_monitor_flags(
    script_present: bool,
    monitor: bool,
    not_before_present: bool,
) -> Result<(), String> {
    if monitor && script_present {
        return Err("--monitor is agent-mode only (a prompt, not a script)".to_string());
    }
    if not_before_present && !monitor {
        return Err("--not-before is only valid with --monitor".to_string());
    }
    Ok(())
}

/// Validate the dispatch-disposition flags at `create` time
/// (headless-agent-automations U2 — R2/R3), the `--not-before` validation
/// pattern: `--headless`/`--paned` are mutually exclusive and agent-only;
/// with `--monitor` both are rejected — `--headless` as redundant (a monitor
/// is unconditionally headless) and `--paned` as contradictory. The socket
/// create arm enforces the same rules on the untrusted wire. Pure; returns
/// the message the CLI prints before exiting 2.
pub fn validate_headless_flags(
    script_present: bool,
    monitor: bool,
    headless: bool,
    paned: bool,
) -> Result<(), String> {
    if headless && paned {
        return Err("pass only one of --headless / --paned".to_string());
    }
    if (headless || paned) && script_present {
        return Err("--headless / --paned are agent-mode only (a prompt, not a script)".to_string());
    }
    if headless && monitor {
        return Err("--headless is redundant with --monitor (a monitor is always headless)".to_string());
    }
    if paned && monitor {
        return Err("--paned contradicts --monitor (a monitor is always headless)".to_string());
    }
    Ok(())
}

/// Monitor-handoff R8: monitors default **Sonnet at xhigh** when `--model` /
/// `--effort` are unspecified; an explicit flag wins per-field. U1–U4 stamp
/// no such default anywhere (the socket create arm passes the request's
/// model/effort through as-is and `resolve_agent_launch` knows nothing of
/// monitors), so this is its home: stamped **CLI-side at create time into
/// the automation's own model/effort slot** — the first link of the existing
/// dispatch-time resolution chain (automation → shared default → Claude
/// default, automations-workspace-and-model U4a). That keeps dispatch
/// monitor-unaware, makes the stored record self-describing (`show`/`list`/
/// dashboard display the real launch values), and deliberately outranks
/// `config.automation_defaults` — a sparse healthcheck must not silently
/// ride a user's expensive default model. No new config surface (the plan's
/// constraint). Pure, unit-tested below.
///
/// The socket create arm (`lib.rs::dispatch_automation_op`) now **backstops**
/// this same default (fix(review) #12): a raw-socket monitor create that
/// bypasses the CLI still lands sonnet/xhigh. The CLI stamp stays anyway —
/// it makes the `--json` output and the local echo self-describing before
/// the socket round-trip; the redundancy is deliberate defense in depth.
pub fn monitor_launch_defaults(
    monitor: bool,
    model: Option<String>,
    effort: Option<String>,
) -> (Option<String>, Option<String>) {
    if !monitor {
        return (model, effort);
    }
    (
        model.or_else(|| Some(MONITOR_DEFAULT_MODEL.to_string())),
        effort.or_else(|| Some(MONITOR_DEFAULT_EFFORT.to_string())),
    )
}

/// Parse a `--not-before` value to epoch ms CLI-side (monitor-handoff U5,
/// R1). Two accepted forms:
///
/// - **RFC3339 with an offset** — `2026-07-12T09:00:00Z`,
///   `2026-07-12T09:00:00+02:00`;
/// - **naive local `"YYYY-MM-DD HH:MM"`** — resolved through `local` (the
///   CLI passes `&chrono::Local`, the system zone; tests pass a fixed
///   [`chrono_tz::Tz`] so DST edges are deterministic). A fall-back fold
///   (two instants share the wall-clock time) takes the **earliest** of the
///   pair — the `schedule.rs` fold contract; a spring-forward gap (the time
///   never exists) is rejected with a pointer at the RFC3339 form.
///
/// A **past** instant is deliberately ACCEPTED: per U2's `advance_from`
/// semantics it clamps to a no-op (the next cron occurrence from now), and
/// refusing would break relaunch-idempotent skill scripts (R10) that re-run
/// the same `create` line after the floor has passed. Pre-epoch instants
/// are rejected — the floor is stored as u64 epoch ms, untrusted numeric
/// input (release overflow-checks are off), so conversions are checked
/// (`try_from`), never `as`-cast.
pub fn parse_not_before<Z: chrono::TimeZone>(input: &str, local: &Z) -> Result<u64, String> {
    let s = input.trim();
    let to_ms = |millis: i64| -> Result<u64, String> {
        u64::try_from(millis)
            .map_err(|_| format!("--not-before {s:?} predates 1970 — it cannot be a floor"))
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return to_ms(dt.timestamp_millis());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return match local.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => to_ms(dt.timestamp_millis()),
            // Fall-back fold: pick the earliest of the duplicated pair
            // (schedule.rs's fold contract — a floor erring an hour early is
            // safe; the cron still gates the actual check instant).
            chrono::LocalResult::Ambiguous(earliest, _) => to_ms(earliest.timestamp_millis()),
            chrono::LocalResult::None => Err(format!(
                "--not-before {s:?} does not exist in local time (a DST spring-forward gap) \
                 — pass an RFC3339 timestamp with an explicit offset instead"
            )),
        };
    }
    Err(format!(
        "--not-before wants an RFC3339 timestamp (e.g. 2026-07-12T09:00:00Z) or a local \
         \"YYYY-MM-DD HH:MM\" (got {s:?})"
    ))
}

/// Parse a `--within` duration (automation-dependencies U5 — R14): bare
/// minutes, or a number suffixed `m`/`h`/`d`. Range-checked against the same
/// bounds the app enforces on the wire ([`crate::automations::depend`]), so
/// the author hears about an out-of-range window before the socket.
pub fn parse_within(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let (num, unit_ms): (&str, u64) = match s.char_indices().last() {
        Some((i, 'm')) => (&s[..i], 60 * 1000),
        Some((i, 'h')) => (&s[..i], 60 * 60 * 1000),
        Some((i, 'd')) => (&s[..i], 24 * 60 * 60 * 1000),
        Some((_, c)) if c.is_ascii_digit() => (s, 60 * 1000), // bare = minutes
        _ => {
            return Err(format!(
                "--within wants a duration like 45m, 2h, 1d, or bare minutes (got {s:?})"
            ))
        }
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("--within wants a duration like 45m, 2h, 1d (got {s:?})"))?;
    crate::automations::depend::validate_within(n.saturating_mul(unit_ms))
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
    /// crash/restart. Set by `create` **and** by `update` (where it means
    /// "turn on" — the off direction rides `clear`, since this field is
    /// skip-if-false and cannot express it). `#[serde(default)]` = `false`
    /// for legacy clients and every other op.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry_on_interrupt: bool,
    /// Monitor flavor (monitor-handoff U4, R1/R11): create-only — still
    /// literally true, and `update` refuses both this field and any update
    /// *to* a monitor (automation-update KTD2: pickup pointers cannot be
    /// re-captured without a registering pane) — agent-mode only (the app
    /// rejects `monitor` + `script`). A monitor create makes
    /// the app capture pickup pointers from the **validated calling pane** —
    /// the wire can never self-declare them — or refuse (R12). U5 sets this
    /// from `--monitor`. `#[serde(default)]`/`skip_serializing_if` keep old
    /// CLI binaries and new servers mutually intelligible (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub monitor: bool,
    /// Monitor not-before floor, epoch ms (monitor-handoff U2/U4, R1):
    /// create-only (monitors are not updatable at all, automation-update
    /// KTD2); clamps every `next_run_at` recompute — including an update's
    /// cron-change recompute, for any record that carries one. Untrusted numeric
    /// input — schedule math is saturating/checked. U5 parses `--not-before`
    /// into it; same back-compat pattern as `monitor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before_ms: Option<u64>,
    /// Dispatch disposition (headless-agent-automations U2 — R1/R2):
    /// agent-mode only; set by `create` and by `update` (whose
    /// `--default-disposition` clears the pin back to `None` via `clear`).
    /// `Some(true)` = `--headless`,
    /// `Some(false)` = `--paned`, `None` (the wire default, and what an old
    /// CLI binary always sends) = follow `config.automation_defaults
    /// .headless` at claim time. Validated CLI-side AND on the untrusted
    /// wire (`--headless`+`--monitor` redundant, `--paned`+`--monitor`
    /// contradictory, either with script agent-only); same back-compat
    /// pattern as `monitor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    /// Dependency edge (automation-dependencies plan, U5 — R14): **set at
    /// create only** — `update` refuses both fields outright and can only
    /// *clear* the stored edge (`--no-after` → `clear`), so the KTD6 cycle
    /// argument holds (automation-update KTD2).
    /// `after` names the upstream automation id; `within_ms` is the optional
    /// symmetric freshness/wait window (absent = the 60-min default). The
    /// untrusted wire is re-validated app-side (existence, non-monitor
    /// upstream, chain depth/cycle, range — U3); same back-compat pattern as
    /// `monitor`: an old server silently ignores the fields and creates an
    /// ungated automation (accepted single-install skew, the plan's R14
    /// note), an old CLI never sends them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ms: Option<u64>,
    /// Verdict gating opt-in (fly-dag-primitives G1): agent-only; when true, a
    /// non-monitor agent run's Succeeded close parses a fenced verdict block
    /// and a dependent gated on it reads PASS/FAIL/DECLINED. Set by `--verdict-gated`
    /// on `create` and `update` (turned off on `update` via the `verdictGated`
    /// clear member). `#[serde(default)]`/skip-if-false keeps old CLI binaries
    /// and new servers mutually intelligible (same back-compat pattern as
    /// `monitor`): an old server ignores it and creates an ungated automation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verdict_gated: bool,
    /// Fields an `automation/update` clears (automation-update U2 — R7,
    /// KTD1). Patch semantics make an absent field mean *unchanged*, which
    /// collides with the create convention where `None` means "use the
    /// default" — so clearing a pinned value gets this explicit channel.
    /// Members come from the closed set
    /// [`crate::automations::UpdateClear::MEMBERS`] (`model`, `effort`,
    /// `disposition`, `retryOnInterrupt`, `after`); an unknown member is
    /// **refused** app-side, never ignored. Notably `retry_on_interrupt`
    /// above is skip-if-false and so cannot express "turn off" — the off
    /// direction rides here and that field's type is unchanged. Ignored by
    /// every other op; `#[serde(default)]`/skip-if-empty keeps old CLI
    /// binaries and new servers mutually intelligible (an *old server*
    /// receiving `automation/update` fails loudly on the unknown op — KTD7).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
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

/// The parsed `fly automation update` flags (automation-update U4, R11).
/// Separated from the arg loop so the contradictory-pair and nothing-to-change
/// refusals are pure and unit-tested — they must fire **before** any socket
/// traffic.
#[derive(Debug, Clone, Default)]
pub struct UpdateFlags {
    pub id: String,
    pub name: Option<String>,
    pub cron: Option<String>,
    pub timezone: Option<String>,
    pub prompt: Option<String>,
    /// Script **content**, already read client-side (R21).
    pub script: Option<String>,
    pub interpreter: Option<String>,
    pub timeout_ms: Option<u64>,
    pub model: Option<String>,
    pub no_model: bool,
    pub effort: Option<String>,
    pub no_effort: bool,
    pub headless: bool,
    pub paned: bool,
    pub default_disposition: bool,
    pub retry_on_interrupt: bool,
    pub no_retry_on_interrupt: bool,
    pub no_after: bool,
    pub verdict_gated: bool,
    pub no_verdict_gated: bool,
}

/// Build the `automation/update` request from parsed flags (R11), or the
/// message the CLI prints before exiting 2. Every check here is re-run
/// app-side on the untrusted wire (KTD6) — this pass exists so a typo costs a
/// message, not a socket round-trip.
pub fn build_update_request(f: UpdateFlags) -> Result<AutomationRequest, String> {
    // Contradictory pairs: a client that asks for both directions of a toggle
    // has a bug, and last-one-wins would hide it.
    if f.model.is_some() && f.no_model {
        return Err("pass only one of --model / --no-model".to_string());
    }
    if f.effort.is_some() && f.no_effort {
        return Err("pass only one of --effort / --no-effort".to_string());
    }
    if f.retry_on_interrupt && f.no_retry_on_interrupt {
        return Err("pass only one of --retry-on-interrupt / --no-retry-on-interrupt".to_string());
    }
    if f.verdict_gated && f.no_verdict_gated {
        return Err("pass only one of --verdict-gated / --no-verdict-gated".to_string());
    }
    if [f.headless, f.paned, f.default_disposition]
        .iter()
        .filter(|b| **b)
        .count()
        > 1
    {
        return Err("pass only one of --headless / --paned / --default-disposition".to_string());
    }
    if let Some(level) = f.effort.as_deref() {
        if !VALID_EFFORTS.contains(&level) {
            return Err(format!(
                "--effort must be one of {} (got {level:?})",
                VALID_EFFORTS.join(", ")
            ));
        }
    }
    if let Some(name) = f.interpreter.as_deref() {
        crate::automations::script::resolve_interpreter(name)?;
    }

    // The closed clear set (KTD1) — the CLI's only job is mapping `--no-*`
    // flags onto its member names.
    let mut clear: Vec<String> = Vec::new();
    if f.no_model {
        clear.push("model".to_string());
    }
    if f.no_effort {
        clear.push("effort".to_string());
    }
    if f.default_disposition {
        clear.push("disposition".to_string());
    }
    if f.no_retry_on_interrupt {
        clear.push("retryOnInterrupt".to_string());
    }
    if f.no_after {
        clear.push("after".to_string());
    }
    if f.no_verdict_gated {
        clear.push("verdictGated".to_string());
    }

    let req = AutomationRequest {
        op: "automation/update".to_string(),
        id: Some(f.id),
        name: f.name,
        cron: f.cron,
        timezone: f.timezone,
        prompt: f.prompt,
        script: f.script,
        interpreter: f.interpreter,
        timeout_ms: f.timeout_ms,
        model: f.model,
        effort: f.effort,
        retry_on_interrupt: f.retry_on_interrupt,
        headless: match (f.headless, f.paned) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        },
        verdict_gated: f.verdict_gated,
        clear,
        ..Default::default()
    };
    // R1: nothing to change is refused here too, so the round-trip is spent
    // only on real edits. Mirrors `UpdateSpec::is_empty` app-side.
    let changes_nothing = req.name.is_none()
        && req.cron.is_none()
        && req.timezone.is_none()
        && req.prompt.is_none()
        && req.script.is_none()
        && req.interpreter.is_none()
        && req.timeout_ms.is_none()
        && req.model.is_none()
        && req.effort.is_none()
        && req.headless.is_none()
        && !req.retry_on_interrupt
        && !req.verdict_gated
        && req.clear.is_empty();
    if changes_nothing {
        return Err("nothing to change — pass at least one field (see --help)".to_string());
    }
    Ok(req)
}

/// Dispatch `fly automation <sub> …`. `args` starts at the subcommand.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("create") => handle_create(&args[1..]),
        Some("update") => handle_update(&args[1..]),
        Some("list") => handle_list(&args[1..]),
        Some("show") => handle_show(&args[1..]),
        Some("runs") => handle_runs(&args[1..]),
        Some("pause") => handle_target("automation/pause", "paused", &args[1..]),
        Some("resume") => handle_target("automation/resume", "resumed", &args[1..]),
        Some("run") => handle_target("automation/run", "run started", &args[1..]),
        Some("delete") => handle_target("automation/delete", "deleted", &args[1..]),
        _ => {
            eprintln!(
                "usage: fly automation \
                 <create|update|list|show|runs|pause|resume|run|delete> …"
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
    let mut monitor = false;
    let mut not_before: Option<String> = None;
    let mut headless = false;
    let mut paned = false;
    let mut after: Option<String> = None;
    let mut within: Option<String> = None;
    let mut verdict_gated = false;
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
                println!("  --interpreter <name>    script interpreter: bash, sh, node, python3 (default: bash)");
                println!("  --timeout <ms>          script timeout in milliseconds (default: 120000)");
                println!("  --retry-on-interrupt    re-run once on the next launch if an app");
                println!("                          crash/restart interrupts a run (default: off)");
                println!("  --headless              agent mode: dispatch closed-loop (claude -p, no");
                println!("                          pane/tab; one prompt in, one captured result out)");
                println!("  --paned                 agent mode: dispatch as an interactive pane");
                println!("                          (the pre-2026-07-31 behavior)");
                println!("  --monitor               agent mode: a parked experiment monitor — sparse");
                println!("                          checks that deliver one PASS/FAIL verdict, then retire");
                println!("  --not-before <time>     monitor only: never check before this time;");
                println!("                          RFC3339 (2026-07-12T09:00:00Z) or local \"YYYY-MM-DD HH:MM\";");
                println!("                          a past time is fine (next cron occurrence from now)");
                println!("  --after <id>            only run against a fresh, successful, not-yet-");
                println!("                          consumed run of automation <id>: a due occurrence");
                println!("                          waits (up to the window) for the upstream to");
                println!("                          succeed, and otherwise records an honest");
                println!("                          'withheld' run saying why it did not fire");
                println!("  --verdict-gated         agent mode: parse a fenced ```verdict block from");
                println!("                          this run's final message (first line PASS, FAIL, or");
                println!("                          DECLINED) — a dependent --after this one fires only");
                println!("                          on PASS, withholds+alerts on FAIL, and withholds");
                println!("                          SILENTLY on DECLINED (ran, nothing to do)");
                println!("  --within <dur>          freshness/wait window for --after (45m, 2h, 1d,");
                println!("                          or bare minutes; default 60m): the upstream");
                println!("                          success may be at most this old, and the");
                println!("                          dependent waits at most this long past its");
                println!("                          scheduled time");
                println!("  --json                  output response as JSON");
                println!();
                println!("Either --prompt (agent mode) or --script/--script-file (script mode) is required.");
                println!("--model / --effort / --monitor / --headless / --paned are agent-mode only.");
                println!("Agent runs dispatch headless by default (config automationDefaults.headless);");
                println!("--headless / --paned pin one automation either way. Monitors are always headless.");
                println!("Monitors default to --model sonnet --effort xhigh and retry-on-interrupt on.");
                println!("A monitor's check must END its final message with one fenced ```verdict block");
                println!("(first line exactly PASS or FAIL, then a note) — a parsed verdict retires the");
                println!("monitor; no block means \"not done yet\". Checks fire only while fly is running.");
                println!();
                println!("Examples:");
                println!("  fly automation create --name 'check tests' --cron '*/5 * * * *' \\");
                println!("    --prompt 'run the test suite'");
                println!("  fly automation create --name 'backup db' --cron '0 2 * * *' \\");
                println!("    --script-file backup.sh");
                println!("  fly automation create --name 'training watch' --cron '0 */6 * * *' \\");
                println!("    --monitor --not-before '2026-07-12 09:00' --prompt 'check the run …'");
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
            "--monitor" => monitor = true,
            "--headless" => headless = true,
            "--paned" => paned = true,
            "--verdict-gated" => verdict_gated = true,
            "--after" => {
                after = it.next().cloned();
                if after.is_none() {
                    eprintln!("fly automation create: --after wants an automation id");
                    return 2;
                }
            }
            "--within" => {
                within = it.next().cloned();
                if within.is_none() {
                    eprintln!("fly automation create: --within wants a duration");
                    return 2;
                }
            }
            "--not-before" => {
                not_before = it.next().cloned();
                if not_before.is_none() {
                    eprintln!("fly automation create: --not-before wants a timestamp");
                    return 2;
                }
            }
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

    // --monitor is agent-only and --not-before is monitor-only
    // (monitor-handoff U5, R1). The help text's verdict-block line is the
    // one-line summary of `automations::verdict::VERDICT_BLOCK_SPEC` — the
    // full contract lives there, nowhere else.
    if let Err(e) = validate_monitor_flags(script.is_some(), monitor, not_before.is_some()) {
        eprintln!("fly automation create: {e}");
        return 2;
    }

    // --headless / --paned: mutually exclusive, agent-only, both rejected
    // with --monitor (headless-agent-automations U2 — R2/R3).
    if let Err(e) = validate_headless_flags(script.is_some(), monitor, headless, paned) {
        eprintln!("fly automation create: {e}");
        return 2;
    }

    // fly-dag-primitives G1: --verdict-gated is agent-only (a script has no
    // assistant turn to parse a verdict from) and pointless on a monitor (a
    // monitor already parses its verdict and retires). Re-validated app-side.
    if verdict_gated && script.is_some() {
        eprintln!("fly automation create: --verdict-gated is agent-mode only (a prompt, not a script)");
        return 2;
    }
    if verdict_gated && monitor {
        eprintln!("fly automation create: --verdict-gated is redundant with --monitor (a monitor already delivers a verdict and retires)");
        return 2;
    }

    // Automation-dependencies U5 (R14): --after is rejected with --monitor
    // (a monitor is a parked experiment, not a pipeline stage), --within is
    // meaningless without --after, and the window parses + range-checks
    // client-side. All re-validated app-side on the untrusted wire (U3).
    if after.is_some() && monitor {
        eprintln!("fly automation create: --after cannot be combined with --monitor");
        return 2;
    }
    if within.is_some() && after.is_none() {
        eprintln!("fly automation create: --within requires --after");
        return 2;
    }
    let within_ms = match &within {
        Some(raw) => match parse_within(raw) {
            Ok(ms) => Some(ms),
            Err(e) => {
                eprintln!("fly automation create: {e}");
                return 2;
            }
        },
        None => None,
    };

    // Parse `--not-before` to epoch ms CLI-side (monitor-handoff U5, R1):
    // RFC3339 with offset, or naive "YYYY-MM-DD HH:MM" resolved through the
    // system-local zone. A past instant is accepted — it clamps to a no-op
    // (monitor-handoff U2); a malformed one is rejected here, before the
    // socket.
    let not_before_ms = match &not_before {
        Some(raw) => match parse_not_before(raw, &chrono::Local) {
            Ok(ms) => Some(ms),
            Err(e) => {
                eprintln!("fly automation create: {e}");
                return 2;
            }
        },
        None => None,
    };

    // Monitor-handoff R8: monitors default Sonnet at xhigh — stamped into the
    // automation's own model/effort slot (see [`monitor_launch_defaults`]);
    // an explicit flag wins per-field.
    let (model, effort) = monitor_launch_defaults(monitor, model, effort);

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
        monitor,
        not_before_ms,
        // Tri-state: an unflagged create sends None and follows the config
        // default at claim time (headless-agent-automations R1/R2).
        headless: match (headless, paned) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        },
        after,
        within_ms,
        verdict_gated,
        ..Default::default()
    };
    send_and_report(req, "created", json)
}

/// `fly automation update <id> [flags]` (automation-update U4 — R11/R12):
/// patch a stored automation in place. Absent flag = unchanged; the `--no-*`
/// flags clear a pin. The exclusions (mode-kind switch, monitors, retired
/// records, setting `--after`, `cwd`) have no flags at all — they are refused
/// app-side with their own messages if a raw socket client tries.
fn handle_update(args: &[String]) -> i32 {
    let mut f = UpdateFlags::default();
    let mut id: Option<String> = None;
    let mut script_file: Option<String> = None;
    let mut json = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("usage: fly automation update <id> [options]");
                println!();
                println!("Patch a stored automation in place. Absent options are left unchanged;");
                println!("the --no-* options clear a pinned value. The id, run history, origin");
                println!("and any dependency edge survive — unlike delete + recreate.");
                println!();
                println!("Options:");
                println!("  --name <name>              rename");
                println!("  --cron <expr>              new cron schedule");
                println!("  --tz, --timezone <tz>      new timezone");
                println!("  --prompt <text>            agent mode: replace the prompt");
                println!("  --model <name>             agent mode: pin the model");
                println!("  --no-model                 agent mode: unpin the model (use the default)");
                println!("  --effort <level>           agent mode: reasoning effort (low, medium, high, xhigh, max)");
                println!("  --no-effort                agent mode: unpin the effort");
                println!("  --headless                 agent mode: pin closed-loop dispatch");
                println!("  --paned                    agent mode: pin interactive-pane dispatch");
                println!("  --default-disposition      agent mode: unpin (follow automationDefaults.headless)");
                println!("  --script <code>            script mode: replace the script content");
                println!("  --script-file <path>       script mode: replace it from a file");
                println!("  --interpreter <name>       script mode: bash, sh, node, python3");
                println!("  --timeout <ms>             script mode: run timeout in milliseconds");
                println!("  --retry-on-interrupt       re-run once after an app crash/restart");
                println!("  --no-retry-on-interrupt    stop re-running after an interrupt");
                println!("  --no-after                 drop the dependency edge (--after)");
                println!("  --verdict-gated            agent mode: parse a fenced ```verdict block (PASS/");
                println!("                             FAIL/DECLINED) so dependents gate on the conclusion");
                println!("  --no-verdict-gated         agent mode: stop parsing a verdict block");
                println!("  --json                     output response as JSON");
                println!();
                println!("Not updatable, by design — delete and recreate instead:");
                println!("  the mode kind (agent ↔ script), the cwd, monitors, retired records,");
                println!("  and setting/re-pointing --after (only --no-after is allowed, since");
                println!("  removing an edge can never create a dependency cycle).");
                println!();
                println!("A cron or timezone change recomputes the next run from now; a PAUSED");
                println!("automation stays paused and picks the new schedule up on resume.");
                println!("A run already in flight keeps the parameters it launched with — the new");
                println!("ones apply from its next run.");
                println!();
                println!("Examples:");
                println!("  fly automation update a1b2c3d4e5 --cron '0 */4 * * *'");
                println!("  fly automation update a1b2c3d4e5 --prompt 'summarize CI, then …' --no-model");
                println!("  fly automation update a1b2c3d4e5 --script-file backup.sh --timeout 300000");
                return 0;
            }
            "--name" => f.name = it.next().cloned(),
            "--cron" => f.cron = it.next().cloned(),
            "--tz" | "--timezone" => f.timezone = it.next().cloned(),
            "--prompt" => f.prompt = it.next().cloned(),
            "--model" => f.model = it.next().cloned(),
            "--no-model" => f.no_model = true,
            "--effort" => f.effort = it.next().cloned(),
            "--no-effort" => f.no_effort = true,
            "--headless" => f.headless = true,
            "--paned" => f.paned = true,
            "--default-disposition" => f.default_disposition = true,
            "--script" => f.script = it.next().cloned(),
            "--script-file" => script_file = it.next().cloned(),
            "--interpreter" => f.interpreter = it.next().cloned(),
            "--timeout" => {
                f.timeout_ms = match it.next().map(|s| s.parse::<u64>()) {
                    Some(Ok(n)) => Some(n),
                    _ => {
                        eprintln!("fly automation update: --timeout wants a millisecond integer");
                        return 2;
                    }
                }
            }
            "--retry-on-interrupt" => f.retry_on_interrupt = true,
            "--no-retry-on-interrupt" => f.no_retry_on_interrupt = true,
            "--no-after" => f.no_after = true,
            "--verdict-gated" => f.verdict_gated = true,
            "--no-verdict-gated" => f.no_verdict_gated = true,
            "--json" => json = true,
            other if !other.starts_with('-') && id.is_none() => id = Some(other.to_string()),
            other => {
                eprintln!("fly automation update: unknown argument {other:?}");
                return 2;
            }
        }
    }

    let Some(id) = id else {
        eprintln!("fly automation update: an automation id is required");
        return 2;
    };
    f.id = id.clone();

    // Read a `--script-file` client-side — the app never opens a client path
    // (R21) — and keep the create path's mutual exclusion.
    if f.script.is_some() && script_file.is_some() {
        eprintln!("fly automation update: pass only one of --script / --script-file");
        return 2;
    }
    if let Some(path) = &script_file {
        match std::fs::read_to_string(path) {
            Ok(content) => f.script = Some(content),
            Err(e) => {
                eprintln!("fly automation update: cannot read --script-file {path:?}: {e}");
                return 1;
            }
        }
    }

    let req = match build_update_request(f) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fly automation update: {e}");
            return 2;
        }
    };
    let code = send_and_report(req, "updated", json);
    // R12: echo the updated record show-style (the store is already flushed by
    // the time the response came back). `--json` stays machine-clean — the
    // response object is the whole output there.
    if code == 0 && !json {
        if let Ok(list) = load_store() {
            if let Some(a) = list.iter().find(|a| a.id == id) {
                let upstream_name = a.after.as_ref().and_then(|e| {
                    list.iter()
                        .find(|u| u.id == e.upstream_id)
                        .map(|u| u.name.clone())
                });
                let headless_default =
                    crate::config::load_with_fallback(&crate::config::default_path())
                        .automation_defaults
                        .headless;
                for line in show_lines(a, now_ms(), headless_default, upstream_name.as_deref()) {
                    println!("{line}");
                }
            }
        }
    }
    code
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
    // Sort mirroring the dashboard (U10, extended by monitor-handoff U5/R18):
    // scheduled rows by next_run ascending — parked monitors ride with
    // recurring automations — then paused (None), then retired last; name
    // tiebreak inside each bucket.
    list.sort_by(|a, b| {
        sort_bucket(a)
            .cmp(&sort_bucket(b))
            .then_with(|| a.next_run_at.cmp(&b.next_run_at))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(list)
}

/// The list/dashboard sort bucket (monitor-handoff U5/R18 mirror):
/// 0 = scheduled (incl. parked monitors, by next-run), 1 = paused,
/// 2 = retired last. Retirement is checked first — [`Automation::retire`]
/// also clears `next_run_at`, and a retired row must never read as merely
/// paused.
fn sort_bucket(a: &Automation) -> u8 {
    if a.retired_at.is_some() {
        2
    } else if a.next_run_at.is_none() {
        1
    } else {
        0
    }
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
        println!("{}", list_line(a, now));
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
    let Some(a) = list.iter().find(|a| a.id == id) else {
        eprintln!("fly automation show: no such automation: {id}");
        return 1;
    };
    if json {
        println!("{}", serde_json::to_string(&a).unwrap_or_default());
        return 0;
    }
    let now = now_ms();
    // Automation-dependencies R15: resolve the upstream's name from the same
    // store read; None flags a dangling edge.
    let upstream_name = a.after.as_ref().and_then(|e| {
        list.iter()
            .find(|u| u.id == e.upstream_id)
            .map(|u| u.name.clone())
    });
    // Best-effort config read (read-only, like the store read): the
    // disposition line names what "default" currently resolves to.
    let headless_default = crate::config::load_with_fallback(&crate::config::default_path())
        .automation_defaults
        .headless;
    for line in show_lines(a, now, headless_default, upstream_name.as_deref()) {
        println!("{line}");
    }
    0
}

fn handle_runs(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage: fly automation runs <id> [--output <run-id>] [-v] [--json]");
        println!();
        println!("List runs for an automation, or show output of a specific run.");
        println!();
        println!("Options:");
        println!("  --output <run-id>  print captured output from a single run");
        println!("  -v, --verbose      also print each run's derived transcript path");
        println!("  --json             output as JSON");
        return 0;
    }
    let json = wants_json(args);
    let mut id: Option<String> = None;
    let mut output_run: Option<String> = None;
    let mut verbose = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => {}
            "-v" | "--verbose" => verbose = true,
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
        println!("{}", run_line(r, now));
        // R10 (headless-agent-automations): the derived transcript path is
        // the deep-dive handle behind the session id — verbose only.
        if verbose {
            if let Some(p) = run_transcript_path(&a, r) {
                println!("    transcript {}", notify::sanitize_title(&p));
            }
        }
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

/// The monitor state column (monitor-handoff U5 — R18's CLI mirror,
/// preceding the U7 dashboard): derived from the stored fields, precedence
/// retired (split pass/fail by the durable verdict row, monitor-handoff R4)
/// > broken (the derived R6/R7 infra-failure count at/past the threshold)
/// > paused > parked. `None` for non-monitors — they render mode labels.
fn monitor_state_label(a: &Automation) -> Option<String> {
    if !a.monitor {
        return None;
    }
    Some(if a.retired_at.is_some() {
        match verdict_run(a).and_then(|r| r.verdict.as_ref()) {
            Some(v) => match v.outcome {
                VerdictOutcome::Pass => "retired pass".to_string(),
                VerdictOutcome::Fail => "retired fail".to_string(),
                // A monitor never retires on DECLINED (fly-dag-primitives G1
                // KTD6) — defensive fallback if a hand-edited store shows one.
                VerdictOutcome::Declined => "retired".to_string(),
            },
            // Defensive: retirement without a surviving verdict row.
            None => "retired".to_string(),
        }
    } else if a.consecutive_infra_failures() >= MONITOR_BROKEN_THRESHOLD {
        "broken".to_string()
    } else if a.next_run_at.is_none() {
        "paused".to_string()
    } else {
        "parked".to_string()
    })
}

/// The newest verdict-bearing run row (monitor-handoff R4: the durable
/// verdict record — eviction-protected in the model). A monitor retires on
/// its first verdict so at most one exists; newest-first keeps the read
/// honest if that invariant ever loosens.
fn verdict_run(a: &Automation) -> Option<&RunRow> {
    a.runs.iter().rev().find(|r| r.verdict.is_some())
}

/// One-line verdict rendering (monitor-handoff U5): the prompt-contract
/// spelling (`PASS`/`FAIL`) plus the sanitized note —
/// [`notify::sanitize_title`] flattens control chars and caps length; the
/// note is captured agent output, untrusted in a terminal.
fn verdict_line(v: &Verdict) -> String {
    let outcome = v.outcome.as_str();
    let note = v.note.trim();
    if note.is_empty() {
        outcome.to_string()
    } else {
        format!("{outcome} — {}", notify::sanitize_title(note))
    }
}

/// The bracketed type column of a `list` row: the mode for ordinary
/// automations, `monitor · <state>` for monitors (monitor-handoff U5).
fn type_label(a: &Automation) -> String {
    match monitor_state_label(a) {
        Some(state) => format!("monitor · {state}"),
        None => mode_label(&a.mode).to_string(),
    }
}

/// The next-run column: a retired monitor never runs again — render `—`,
/// not `paused` (monitor-handoff R3); a dependent whose due occurrence is
/// currently deferring renders `waiting on upstream` (automation-
/// dependencies R15 — its `next_run_at` sits in the past by design while
/// the sweep re-evaluates every tick, so a relative label would misread as
/// "overdue"); everything else keeps the existing paused/relative labels.
fn next_label(a: &Automation, now: u64) -> String {
    if a.retired_at.is_some() {
        return "—".to_string();
    }
    if a.after.is_some() && a.enabled && a.next_run_at.is_some_and(|t| t <= now) {
        return "waiting on upstream".to_string();
    }
    next_run_label(a.next_run_at, now)
}

/// One `list` row (pure — testable without stdout). A dependent carries its
/// edge inline (automation-dependencies R15).
fn list_line(a: &Automation, now: u64) -> String {
    let after = a
        .after
        .as_ref()
        .map(|e| format!("  after:{}", e.upstream_id))
        .unwrap_or_default();
    format!(
        "{}  {}  [{}]{}  {} {}  next {}  last {}",
        a.id,
        notify::sanitize_title(&a.name),
        type_label(a),
        after,
        a.cron,
        a.timezone,
        next_label(a, now),
        last_run_label(a),
    )
}

/// The `show` body (pure — testable without stdout): one aligned
/// `key value` line per field. Monitor lines (monitor-handoff U5, R4/R18)
/// render only for monitors: the state (with the not-before floor while
/// parked, or the retirement instant), the durable verdict + bundle path
/// when present, and — while the monitor is still live — the missed-tick
/// caveat the plan requires `show` to state (checks fire only while fly
/// runs; no catch-up).
fn show_lines(
    a: &Automation,
    now: u64,
    headless_default: bool,
    upstream_name: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![
        format!("id        {}", a.id),
        format!("name      {}", notify::sanitize_title(&a.name)),
        format!("mode      {}", mode_label(&a.mode)),
        format!("schedule  {} {}", a.cron, a.timezone),
        format!("cwd       {}", notify::sanitize_title(&a.cwd)),
        format!("enabled   {}", a.enabled),
        format!(
            "retry     {}",
            if a.retry_on_interrupt { "on interrupt" } else { "off" }
        ),
    ];
    // Automation-dependencies R15: the dependency edge — upstream id (+ name
    // when it still resolves; a dangling edge is flagged, since the sweep
    // will withhold on it) and the effective window.
    if let Some(edge) = &a.after {
        let who = match upstream_name {
            Some(n) => format!("{} ({})", edge.upstream_id, notify::sanitize_title(n)),
            None => format!("{} (MISSING — runs will be withheld)", edge.upstream_id),
        };
        lines.push(format!(
            "after     {who} — requires upstream success within {}m",
            edge.within() / 60_000
        ));
    }
    // Dispatch disposition (headless-agent-automations U2 — R10's display
    // half): explicit pin, or the config default it follows. Monitors skip
    // it — they are unconditionally headless and say so via their own lines.
    if !a.monitor {
        if let Mode::Agent { headless, .. } = &a.mode {
            let disposition = match headless {
                Some(true) => "headless".to_string(),
                Some(false) => "paned".to_string(),
                None => format!(
                    "default ({})",
                    if headless_default { "headless" } else { "paned" }
                ),
            };
            lines.push(format!("dispatch  {disposition}"));
        }
    }
    if let Some(state) = monitor_state_label(a) {
        let mut line = state;
        if let Some(t) = a.retired_at {
            line.push_str(&format!(" ({})", rel_label(t, now)));
        } else if let Some(nb) = a.not_before_ms.filter(|nb| *nb > now) {
            line.push_str(&format!(" (not before {})", rel_label(nb, now)));
        }
        lines.push(format!("monitor   {line}"));
        if let Some(row) = verdict_run(a) {
            if let Some(v) = row.verdict.as_ref() {
                lines.push(format!("verdict   {}", verdict_line(v)));
            }
            if let Some(b) = &row.bundle_path {
                lines.push(format!("bundle    {}", notify::sanitize_title(b)));
            }
        }
        if a.retired_at.is_none() {
            lines.push(
                "note      checks fire only while fly is running — missed ticks are not \
                 caught up"
                    .to_string(),
            );
        }
    }
    lines.push(format!("next run  {}", next_label(a, now)));
    lines.push(format!("last run  {}", last_run_label(a)));
    lines.push(format!("runs      {} in history", a.runs.len()));
    lines
}

/// One `runs` row (pure — testable without stdout): id, status, when, error
/// — plus, for verdict-bearing rows (monitor-handoff U5), the bracketed
/// verdict column and the bundle path when present.
fn run_line(r: &RunRow, now: u64) -> String {
    let when = r.started_at.or(r.finished_at);
    let mut line = format!(
        "{}  {:<9}  {}  {}",
        r.id,
        status_label(r.status),
        when.map(|t| rel_label(t, now)).unwrap_or_else(|| "—".to_string()),
        r.error
            .as_deref()
            .map(notify::sanitize_title)
            .unwrap_or_default(),
    );
    if let Some(v) = &r.verdict {
        line.push_str(&format!("  [{}]", verdict_line(v)));
    }
    if let Some(b) = &r.bundle_path {
        line.push_str(&format!("  bundle {}", notify::sanitize_title(b)));
    }
    // Headless-agent-automations R10: the closed-loop debugging handle — a
    // headless run has no pane scrollback, so the stream-stamped session id
    // is how you get back to it (`claude --resume <id>` works on it).
    if let Some(sid) = &r.session_id {
        line.push_str(&format!("  session {}", notify::sanitize_title(sid)));
    }
    line
}

/// Headless-agent-automations R10 (`runs -v`): the run's derived transcript
/// path — `<claude projects root>/<encoded cwd>/<session id>.jsonl` — the
/// deep-dive handle behind the session id. `None` when the run has no
/// stamped session id or no home dir resolves.
fn run_transcript_path(a: &Automation, r: &RunRow) -> Option<String> {
    let sid = r.session_id.as_deref()?;
    let root = crate::session::transcript::claude_projects_root()?;
    let dir = crate::session::transcript::encode_cwd(&a.cwd);
    Some(root.join(dir).join(format!("{sid}.jsonl")).display().to_string())
}

fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
        // Automation-dependencies R15: the honest dependency decline; the
        // reason rides the row's error column like a skip reason.
        RunStatus::Withheld => "withheld",
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
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            after: None,
            verdict_gated: false,
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

    // ---- monitor flags + not-before parsing (monitor-handoff U5) ------------

    // monitor-handoff R1: --monitor is agent-only; --not-before is
    // monitor-only; the valid combinations pass.
    #[test]
    fn validate_monitor_flags_rejects_script_monitor_and_orphan_not_before() {
        let m = validate_monitor_flags(true, true, false).unwrap_err();
        assert!(m.contains("agent-mode"), "monitor+script rejected: {m}");

        let m = validate_monitor_flags(false, false, true).unwrap_err();
        assert!(m.contains("--monitor"), "orphan --not-before rejected: {m}");

        assert!(validate_monitor_flags(false, false, false).is_ok());
        assert!(validate_monitor_flags(true, false, false).is_ok(), "plain script fine");
        assert!(validate_monitor_flags(false, true, false).is_ok(), "monitor sans floor fine");
        assert!(validate_monitor_flags(false, true, true).is_ok(), "monitor + floor fine");
    }

    // Headless-agent-automations U2 (R2/R3): --headless/--paned are mutually
    // exclusive and agent-only; with --monitor, --headless is rejected as
    // redundant and --paned as contradictory; the valid combinations pass.
    #[test]
    fn validate_headless_flags_rejects_the_bad_combinations() {
        let m = validate_headless_flags(false, false, true, true).unwrap_err();
        assert!(m.contains("only one"), "mutually exclusive: {m}");

        let m = validate_headless_flags(true, false, true, false).unwrap_err();
        assert!(m.contains("agent-mode"), "headless+script rejected: {m}");
        let m = validate_headless_flags(true, false, false, true).unwrap_err();
        assert!(m.contains("agent-mode"), "paned+script rejected: {m}");

        let m = validate_headless_flags(false, true, true, false).unwrap_err();
        assert!(m.contains("redundant"), "headless+monitor rejected: {m}");
        let m = validate_headless_flags(false, true, false, true).unwrap_err();
        assert!(m.contains("contradicts"), "paned+monitor rejected: {m}");

        assert!(validate_headless_flags(false, false, false, false).is_ok());
        assert!(validate_headless_flags(false, false, true, false).is_ok());
        assert!(validate_headless_flags(false, false, false, true).is_ok());
        assert!(validate_headless_flags(false, true, false, false).is_ok(), "plain monitor fine");
        assert!(validate_headless_flags(true, false, false, false).is_ok(), "plain script fine");
    }

    // Headless-agent-automations U2 (R12): wire back-compat — an old CLI's
    // request JSON has no `headless` key and must load as None (follow the
    // config default), and a new CLI's unflagged create serializes without
    // the key so an old server ignores nothing it can see.
    #[test]
    fn request_headless_is_back_compat_tri_state_on_the_wire() {
        let old: AutomationRequest =
            serde_json::from_str(r#"{"token":"t","op":"automation/create"}"#).unwrap();
        assert_eq!(old.headless, None);

        let unflagged = AutomationRequest {
            op: "automation/create".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&unflagged).unwrap();
        assert!(v.get("headless").is_none(), "None is omitted, not null");

        let paned = AutomationRequest {
            headless: Some(false),
            ..Default::default()
        };
        let v = serde_json::to_value(&paned).unwrap();
        assert_eq!(v["headless"], false, "the explicit override rides the wire");
    }

    // monitor-handoff R8: monitors default sonnet/xhigh per-field; explicit
    // flags win; non-monitors are untouched (their None flows to the shared
    // config default at dispatch, as before).
    #[test]
    fn monitor_launch_defaults_fill_sonnet_xhigh_per_field_for_monitors_only() {
        assert_eq!(
            monitor_launch_defaults(true, None, None),
            (Some("sonnet".into()), Some("xhigh".into()))
        );
        assert_eq!(
            monitor_launch_defaults(true, Some("opus".into()), None),
            (Some("opus".into()), Some("xhigh".into())),
            "explicit model wins; effort still defaults"
        );
        assert_eq!(
            monitor_launch_defaults(true, None, Some("high".into())),
            (Some("sonnet".into()), Some("high".into())),
            "explicit effort wins; model still defaults"
        );
        assert_eq!(
            monitor_launch_defaults(false, None, None),
            (None, None),
            "non-monitors keep None (shared default resolves at dispatch)"
        );
        assert!(
            VALID_EFFORTS.contains(&MONITOR_DEFAULT_EFFORT),
            "the R8 default effort is a member of the closed set"
        );
    }

    // monitor-handoff U5/R1: both accepted timestamp forms parse to the right
    // epoch ms — RFC3339 with Z or an explicit offset, and the naive local
    // form resolved through the injected zone (deterministic in tests).
    #[test]
    fn parse_not_before_accepts_rfc3339_and_naive_local_forms() {
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        assert_eq!(
            parse_not_before("2026-07-12T09:00:00Z", &chrono::Utc),
            Ok(1_783_846_800_000)
        );
        assert_eq!(
            parse_not_before("2026-07-12T09:00:00+02:00", &chrono::Utc),
            Ok(1_783_839_600_000),
            "the explicit offset is honored"
        );
        // RFC3339 carries its own offset — the local zone is irrelevant.
        assert_eq!(
            parse_not_before("2026-07-12T09:00:00Z", &ny),
            Ok(1_783_846_800_000)
        );
        // Naive local: 09:00 in July New York is EDT (UTC-4) → 13:00Z.
        assert_eq!(
            parse_not_before("2026-07-12 09:00", &ny),
            Ok(1_783_861_200_000)
        );
        assert_eq!(
            parse_not_before(" 2026-07-12 09:00 ", &chrono::Utc),
            Ok(1_783_846_800_000),
            "surrounding whitespace is trimmed"
        );
    }

    // monitor-handoff U5: a PAST instant is accepted (it clamps to a no-op
    // per U2 — refusing would break relaunch-idempotent skill scripts, R10);
    // garbage, pre-epoch instants, and a DST-gap local time are rejected
    // with pointed messages; a fall-back fold takes the earliest of the pair.
    #[test]
    fn parse_not_before_accepts_past_and_rejects_garbage_pre_epoch_and_dst_gap() {
        let ny: chrono_tz::Tz = "America/New_York".parse().unwrap();
        assert_eq!(
            parse_not_before("1999-01-01T00:00:00Z", &chrono::Utc),
            Ok(915_148_800_000),
            "a past floor parses fine — it is a scheduling no-op, not an error"
        );

        let m = parse_not_before("next tuesday", &chrono::Utc).unwrap_err();
        assert!(m.contains("RFC3339"), "names the RFC3339 form: {m}");
        assert!(m.contains("YYYY-MM-DD HH:MM"), "names the naive form: {m}");
        assert!(m.contains("next tuesday"), "echoes the bad value: {m}");

        let m = parse_not_before("1960-01-01T00:00:00Z", &chrono::Utc).unwrap_err();
        assert!(m.contains("1970"), "pre-epoch cannot be a u64 floor: {m}");

        // 2026-03-08 02:30 never exists in New York (spring-forward gap).
        let m = parse_not_before("2026-03-08 02:30", &ny).unwrap_err();
        assert!(m.contains("does not exist"), "DST gap rejected: {m}");

        // 2026-11-01 01:30 exists twice (fall-back fold): the earliest wins
        // (EDT, UTC-4 → 05:30Z), matching schedule.rs's fold contract.
        assert_eq!(
            parse_not_before("2026-11-01 01:30", &ny),
            Ok(1_793_511_000_000)
        );
    }

    // ---- monitor rendering (monitor-handoff U5, R18's CLI mirror) ------------

    /// An agent-mode monitor fixture over [`super_automation`].
    fn monitor_automation(id: &str, name: &str) -> Automation {
        let mut a = super_automation(id, name);
        a.mode = Mode::Agent {
            prompt: "check the run".into(),
            model: Some("sonnet".into()),
            effort: Some("xhigh".into()), headless: None,
        };
        a.monitor = true;
        a
    }

    /// A terminal run row for rendering tests (fields are pub; building
    /// directly keeps the fixtures independent of claim/close mechanics).
    fn run_row(id: &str, status: RunStatus) -> RunRow {
        RunRow {
            id: id.into(),
            mode: crate::automations::model::RunMode::Agent,
            trigger: crate::automations::model::Trigger::Schedule,
            status,
            pane_id: None,
            model: None,
            effort: None,
            verdict: None,
            bundle_path: None,
            headless: false,
            session_id: None,
            upstream_run_id: None,
            output: None,
            exit_code: None,
            error: None,
            scheduled_for: None,
            started_at: Some(1_000),
            finished_at: Some(2_000),
        }
    }

    // monitor-handoff R18 (CLI mirror): every monitor state derives from the
    // stored fields with the documented precedence; non-monitors get None.
    #[test]
    fn monitor_state_label_derives_all_five_states() {
        let plain = super_automation("p", "plain");
        assert_eq!(monitor_state_label(&plain), None, "non-monitor: no state");

        let parked = monitor_automation("m1", "parked");
        assert_eq!(monitor_state_label(&parked).as_deref(), Some("parked"));

        let mut paused = monitor_automation("m2", "paused");
        paused.next_run_at = None;
        assert_eq!(monitor_state_label(&paused).as_deref(), Some("paused"));

        let mut broken = monitor_automation("m3", "broken");
        for i in 0..MONITOR_BROKEN_THRESHOLD {
            broken.runs.push(run_row(&format!("f{i}"), RunStatus::Failed));
        }
        assert_eq!(
            monitor_state_label(&broken).as_deref(),
            Some("broken"),
            "3 trailing verdict-less failures read broken (R6/R7)"
        );

        let mut pass = monitor_automation("m4", "pass");
        let mut row = run_row("v", RunStatus::Succeeded);
        row.verdict = Some(Verdict {
            outcome: VerdictOutcome::Pass,
            note: "converged".into(),
        });
        pass.runs.push(row);
        pass.retired_at = Some(3_000);
        pass.next_run_at = None;
        assert_eq!(monitor_state_label(&pass).as_deref(), Some("retired pass"));

        let mut fail = monitor_automation("m5", "fail");
        let mut row = run_row("v", RunStatus::Failed);
        row.verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "experiment died".into(),
        });
        fail.runs.push(row);
        fail.retired_at = Some(3_000);
        fail.next_run_at = None;
        assert_eq!(monitor_state_label(&fail).as_deref(), Some("retired fail"));

        // Defensive: retirement outranks a broken-looking history, and a
        // retired monitor with no surviving verdict row still reads retired.
        let mut bare = monitor_automation("m6", "bare");
        bare.retired_at = Some(3_000);
        bare.next_run_at = None;
        for i in 0..MONITOR_BROKEN_THRESHOLD {
            bare.runs.push(run_row(&format!("f{i}"), RunStatus::Failed));
        }
        assert_eq!(monitor_state_label(&bare).as_deref(), Some("retired"));
    }

    // monitor-handoff R4/R18: `show` on a retired monitor displays the
    // verdict (and the bundle path when present); the retired row renders no
    // missed-tick note and a `—` next run.
    #[test]
    fn show_lines_render_a_retired_monitors_verdict_and_bundle() {
        let now = 1_000_000_000u64;
        let mut a = monitor_automation("m1", "training watch");
        let mut row = run_row("v", RunStatus::Failed);
        row.verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "experiment \x1b[31mdied".into(),
        });
        row.bundle_path = Some("/data/monitor-bundles/m1-v.md".into());
        a.runs.push(row);
        a.retired_at = Some(now - 3_600_000);
        a.next_run_at = None;

        let lines = show_lines(&a, now, true, None);
        assert!(
            lines.iter().any(|l| l == "monitor   retired fail (1h ago)"),
            "state + retirement instant: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "verdict   FAIL — experiment [31mdied"),
            "verdict rendered, note control-sanitized: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "bundle    /data/monitor-bundles/m1-v.md"),
            "bundle path rendered: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "next run  —"),
            "a retired monitor never runs again — never 'paused': {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("note ")),
            "the missed-tick caveat is for live monitors only: {lines:?}"
        );
    }

    // monitor-handoff R1/R18: `show` on a parked monitor renders the state
    // with its not-before floor and the missed-tick caveat; a plain
    // automation renders none of the monitor lines.
    #[test]
    fn show_lines_render_parked_floor_and_note_and_skip_non_monitors() {
        let now = 1_000_000_000u64;
        let mut parked = monitor_automation("m1", "watch");
        parked.not_before_ms = Some(now + 2 * 3_600_000);
        parked.next_run_at = Some(now + 2 * 3_600_000);

        let lines = show_lines(&parked, now, true, None);
        assert!(
            lines.iter().any(|l| l == "monitor   parked (not before in 2h)"),
            "parked state carries the floor: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("note      checks fire only while fly is running")),
            "the plan's missed-tick caveat is stated by show: {lines:?}"
        );
        assert!(!lines.iter().any(|l| l.starts_with("verdict")), "no verdict yet");

        let plain = super_automation("p", "plain");
        let lines = show_lines(&plain, now, true, None);
        for prefix in ["monitor", "verdict", "bundle", "note"] {
            assert!(
                !lines.iter().any(|l| l.starts_with(prefix)),
                "non-monitor show has no {prefix} line: {lines:?}"
            );
        }
        assert!(lines.iter().any(|l| l.starts_with("next run  ")));
    }

    // Headless-agent-automations U2 (R10's display half): `show` renders the
    // dispatch disposition for non-monitor agent automations — the explicit
    // pin, or what "default" currently resolves to — and skips it for
    // scripts (no disposition) and monitors (unconditionally headless,
    // stated by their own lines).
    #[test]
    fn show_lines_render_the_dispatch_disposition_for_plain_agents_only() {
        let now = 1_000_000_000u64;
        let agent = |headless: Option<bool>| {
            let mut a = super_automation("a", "agent");
            a.mode = Mode::Agent {
                prompt: "audit".into(),
                model: None,
                effort: None,
                headless,
            };
            a
        };
        let lines = show_lines(&agent(None), now, true, None);
        assert!(
            lines.iter().any(|l| l == "dispatch  default (headless)"),
            "unpinned names the resolved default: {lines:?}"
        );
        let lines = show_lines(&agent(None), now, false, None);
        assert!(lines.iter().any(|l| l == "dispatch  default (paned)"), "{lines:?}");
        let lines = show_lines(&agent(Some(true)), now, false, None);
        assert!(lines.iter().any(|l| l == "dispatch  headless"), "{lines:?}");
        let lines = show_lines(&agent(Some(false)), now, true, None);
        assert!(lines.iter().any(|l| l == "dispatch  paned"), "{lines:?}");

        let script = super_automation("s", "script");
        let lines = show_lines(&script, now, true, None);
        assert!(!lines.iter().any(|l| l.starts_with("dispatch")), "{lines:?}");
        let monitor = monitor_automation("m", "watch");
        let lines = show_lines(&monitor, now, true, None);
        assert!(
            !lines.iter().any(|l| l.starts_with("dispatch")),
            "a monitor states headless via its own lines: {lines:?}"
        );
    }

    // Headless-agent-automations R10: a run row with a stamped session id
    // renders it (the closed-loop debugging handle); pane rows (no id) are
    // unchanged. The derived transcript path is the `-v` add-on, built from
    // the automation's cwd + the id.
    #[test]
    fn run_line_renders_the_stamped_session_id() {
        let now = 1_000_000_000u64;
        let mut row = run_row("r1", RunStatus::Succeeded);
        assert!(!run_line(&row, now).contains("session"), "pane rows unchanged");
        row.headless = true;
        row.session_id = Some("sess-42".into());
        let line = run_line(&row, now);
        assert!(line.contains("session sess-42"), "{line}");

        let a = super_automation("a", "x");
        assert_eq!(run_transcript_path(&a, &run_row("r2", RunStatus::Succeeded)), None);
        if let Some(p) = run_transcript_path(&a, &row) {
            assert!(p.ends_with("sess-42.jsonl"), "{p}");
            assert!(p.contains("-tmp"), "encoded cwd rides the path: {p}");
        }
    }

    // monitor-handoff R18 (CLI mirror): list rows carry the monitor state in
    // the type column and `—` for a retired next-run; runs rows carry the
    // verdict + bundle columns.
    #[test]
    fn list_line_and_run_line_render_monitor_columns() {
        let now = 1_000_000_000u64;
        let mut parked = monitor_automation("m1", "watch");
        parked.next_run_at = Some(now + 3_600_000);
        let line = list_line(&parked, now);
        assert!(line.contains("[monitor · parked]"), "type column: {line}");
        assert!(line.contains("next in 1h"), "{line}");

        let mut retired = monitor_automation("m2", "done");
        let mut row = run_row("v", RunStatus::Succeeded);
        row.verdict = Some(Verdict {
            outcome: VerdictOutcome::Pass,
            note: "converged".into(),
        });
        retired.runs.push(row);
        retired.retired_at = Some(now);
        retired.next_run_at = None;
        let line = list_line(&retired, now);
        assert!(line.contains("[monitor · retired pass]"), "{line}");
        assert!(line.contains("next —"), "retired is not 'paused': {line}");

        let plain_line = list_line(&super_automation("p", "plain"), now);
        assert!(plain_line.contains("[script]"), "mode label unchanged: {plain_line}");

        let mut vrow = run_row("r9", RunStatus::Failed);
        vrow.verdict = Some(Verdict {
            outcome: VerdictOutcome::Fail,
            note: "experiment died".into(),
        });
        vrow.bundle_path = Some("/data/b.md".into());
        let line = run_line(&vrow, now);
        assert!(line.contains("[FAIL — experiment died]"), "{line}");
        assert!(line.contains("bundle /data/b.md"), "{line}");
        assert!(
            !run_line(&run_row("r0", RunStatus::Succeeded), now).contains('['),
            "verdict-less rows render no verdict column"
        );
    }

    // monitor-handoff R18: retired monitors sort last — after paused — while
    // a parked monitor rides with recurring automations by next-run (the
    // dashboard order the CLI mirrors).
    #[test]
    fn load_store_at_sorts_parked_with_recurring_and_retired_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let s = store::Store::load_at(path.clone(), dir.path().join("scripts"));
        s.mutate(|map| {
            let mut recurring = super_automation("b", "beta");
            recurring.next_run_at = Some(3_000);
            map.insert("b".into(), recurring);

            let mut parked = monitor_automation("m", "parked-monitor");
            parked.next_run_at = Some(1_000);
            map.insert("m".into(), parked);

            let mut paused = super_automation("p", "paused");
            paused.next_run_at = None;
            map.insert("p".into(), paused);

            let mut retired = monitor_automation("r", "retired-monitor");
            retired.retired_at = Some(9_000);
            retired.next_run_at = None;
            map.insert("r".into(), retired);
        })
        .unwrap();

        let order: Vec<String> = load_store_at(&path)
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(
            order,
            ["m", "b", "p", "r"],
            "parked by next-run with recurring, then paused, then retired"
        );
    }

    // ---- automation-dependencies (U5) -----------------------------------------

    // R14: --within durations parse as bare minutes or m/h/d, range-checked
    // against the app's own bounds; garbage rejects.
    #[test]
    fn parse_within_accepts_minutes_hours_days_and_rejects_garbage() {
        assert_eq!(parse_within("45"), Ok(45 * 60 * 1000));
        assert_eq!(parse_within("45m"), Ok(45 * 60 * 1000));
        assert_eq!(parse_within("2h"), Ok(2 * 60 * 60 * 1000));
        assert_eq!(parse_within("1d"), Ok(24 * 60 * 60 * 1000));
        assert!(parse_within("0m").is_err(), "below the 1-min floor");
        assert!(parse_within("8d").is_err(), "above the 7-day ceiling");
        assert!(parse_within("soon").is_err());
        assert!(parse_within("").is_err());
    }

    // R14 wire back-compat (the monitor-field pattern): a request without
    // the dependency fields serializes without the keys — an old server
    // sees yesterday's shape — and one carrying them round-trips.
    #[test]
    fn request_after_fields_are_back_compat_on_the_wire() {
        let bare = AutomationRequest {
            op: "automation/create".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&bare).unwrap();
        assert!(v.get("after").is_none(), "absent field omitted entirely");
        assert!(v.get("within_ms").is_none());

        let with = AutomationRequest {
            op: "automation/create".into(),
            after: Some("up1".into()),
            within_ms: Some(120_000),
            ..Default::default()
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["after"], "up1");
        // The request envelope is snake_case (unlike the store/model shape).
        assert_eq!(v["within_ms"], 120_000);
        let back: AutomationRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back.after.as_deref(), Some("up1"));
        assert_eq!(back.within_ms, Some(120_000));

        // An old client's payload (no keys) still parses with both None.
        let old: AutomationRequest =
            serde_json::from_str(r#"{"token":"t","op":"automation/create"}"#).unwrap();
        assert_eq!(old.after, None);
        assert_eq!(old.within_ms, None);
    }

    // R15: list marks the edge and renders `waiting on upstream` for a
    // due-and-deferring dependent; show prints the edge with the window and
    // flags a dangling upstream; a withheld run row renders its status and
    // reason.
    #[test]
    fn dependency_rendering_in_list_show_and_runs() {
        use crate::automations::model::Dependency;
        let now = 1_000_000_000u64;
        let mut a = super_automation("d1", "feed analysis");
        a.after = Some(Dependency {
            upstream_id: "up1".into(),
            within_ms: Some(30 * 60 * 1000),
        });

        // Future occurrence: the ordinary relative label, plus the edge tag.
        a.next_run_at = Some(now + 60_000);
        let line = list_line(&a, now);
        assert!(line.contains("after:up1"), "got: {line}");
        assert!(!line.contains("waiting on upstream"), "got: {line}");

        // Due (deferring): the waiting label replaces the misleading
        // "overdue" relative time.
        a.next_run_at = Some(now - 60_000);
        let line = list_line(&a, now);
        assert!(line.contains("next waiting on upstream"), "got: {line}");
        // …but not when paused (next_run_at None renders paused as before).
        a.next_run_at = None;
        assert!(list_line(&a, now).contains("next paused"));

        // show: resolved upstream name + window; dangling edge flagged.
        a.next_run_at = Some(now + 60_000);
        let lines = show_lines(&a, now, true, Some("modal feed"));
        let after_line = lines.iter().find(|l| l.starts_with("after")).unwrap();
        assert!(after_line.contains("up1 (modal feed)"), "got: {after_line}");
        assert!(after_line.contains("within 30m"), "got: {after_line}");
        let lines = show_lines(&a, now, true, None);
        let after_line = lines.iter().find(|l| l.starts_with("after")).unwrap();
        assert!(after_line.contains("MISSING"), "got: {after_line}");

        // runs: a withheld row renders the status label + its reason.
        let mut row = run_row("w1", RunStatus::Withheld);
        row.error = Some("upstream failed (exit 1)".into());
        row.started_at = None;
        let line = run_line(&row, now);
        assert!(line.contains("withheld"), "got: {line}");
        assert!(line.contains("upstream failed (exit 1)"), "got: {line}");
    }

    // ---- automation-update U4 (R11) ----------------------------------------

    fn update_flags(id: &str) -> UpdateFlags {
        UpdateFlags {
            id: id.to_string(),
            ..Default::default()
        }
    }

    // R11: contradictory pairs and a zero-change invocation are CLI errors
    // *before* any socket traffic — the round-trip is spent only on real edits.
    #[test]
    fn update_rejects_contradictory_pairs_and_empty_edits_before_the_socket() {
        let cases: Vec<(UpdateFlags, &str)> = vec![
            (update_flags("a1"), "nothing to change"),
            (
                UpdateFlags {
                    model: Some("opus".into()),
                    no_model: true,
                    ..update_flags("a1")
                },
                "--model / --no-model",
            ),
            (
                UpdateFlags {
                    effort: Some("high".into()),
                    no_effort: true,
                    ..update_flags("a1")
                },
                "--effort / --no-effort",
            ),
            (
                UpdateFlags {
                    retry_on_interrupt: true,
                    no_retry_on_interrupt: true,
                    ..update_flags("a1")
                },
                "--retry-on-interrupt / --no-retry-on-interrupt",
            ),
            (
                UpdateFlags {
                    headless: true,
                    paned: true,
                    ..update_flags("a1")
                },
                "--headless / --paned / --default-disposition",
            ),
            (
                UpdateFlags {
                    paned: true,
                    default_disposition: true,
                    ..update_flags("a1")
                },
                "--headless / --paned / --default-disposition",
            ),
            (
                UpdateFlags {
                    effort: Some("extreme".into()),
                    ..update_flags("a1")
                },
                "--effort must be one of",
            ),
            (
                UpdateFlags {
                    interpreter: Some("perl".into()),
                    ..update_flags("a1")
                },
                "interpreter",
            ),
        ];
        for (flags, expect) in cases {
            let err = build_update_request(flags.clone()).unwrap_err();
            assert!(err.contains(expect), "got {err:?} for {flags:?}");
        }
    }

    // KTD1: `--no-*` flags map onto the closed clear set, and both directions
    // of the retry toggle are expressible (the wire bool is skip-if-false, so
    // "off" can only ride `clear`).
    #[test]
    fn update_maps_no_flags_onto_the_clear_set_and_pins_onto_the_fields() {
        let req = build_update_request(UpdateFlags {
            no_model: true,
            no_effort: true,
            default_disposition: true,
            no_retry_on_interrupt: true,
            no_after: true,
            ..update_flags("a1")
        })
        .unwrap();
        assert_eq!(req.op, "automation/update");
        assert_eq!(req.id.as_deref(), Some("a1"));
        assert_eq!(
            req.clear,
            vec!["model", "effort", "disposition", "retryOnInterrupt", "after"]
        );
        assert!(!req.retry_on_interrupt, "off rides `clear`, not the bool");
        assert_eq!(req.headless, None, "clearing the pin is not pinning `paned`");
        // Every emitted member is one the app accepts (the two sides of KTD1's
        // closed set cannot drift).
        crate::automations::UpdateClear::parse(&req.clear).expect("members are known app-side");

        let req = build_update_request(UpdateFlags {
            model: Some("opus".into()),
            effort: Some("xhigh".into()),
            paned: true,
            retry_on_interrupt: true,
            name: Some("nightly".into()),
            ..update_flags("a1")
        })
        .unwrap();
        assert!(req.clear.is_empty());
        assert_eq!(req.model.as_deref(), Some("opus"));
        assert_eq!(req.effort.as_deref(), Some("xhigh"));
        assert_eq!(req.headless, Some(false), "--paned pins the pane disposition");
        assert!(req.retry_on_interrupt);
        assert_eq!(req.name.as_deref(), Some("nightly"));
        // Update never carries the create-only fields (each refused app-side).
        assert_eq!(req.cwd, None);
        assert_eq!(req.after, None);
        assert_eq!(req.not_before_ms, None);
        assert!(!req.monitor);
    }

    // Back-compat (KTD7): the additive `clear` field is skip-if-empty, so
    // every existing op serializes byte-identically for old servers.
    #[test]
    fn an_empty_clear_list_never_appears_on_the_wire() {
        let req = AutomationRequest {
            op: "automation/pause".into(),
            id: Some("a1".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("clear"), "got: {json}");
        // …and an old CLI's payload (no `clear` key) still deserializes.
        let back: AutomationRequest =
            serde_json::from_str(r#"{"token":"t","op":"automation/update","id":"a1"}"#).unwrap();
        assert!(back.clear.is_empty());
    }
}
