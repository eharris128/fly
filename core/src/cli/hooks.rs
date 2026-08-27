//! `fly hooks setup` / `fly hooks teardown` (U9).
//!
//! Idempotently writes (or removes) fly's command hooks in Claude Code's
//! `settings.json`. Mutations merge under fly-owned blocks, preserve every
//! other key, back up the file once before first modification, and record the
//! absolute canonical `fly` binary path (never a PATH-relative name).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::state::attention::Reason;

/// What a fly hook on a Claude Code event does (fix-session-pane-attribution
/// U7; hook-ask-channel U5): raise attention with a CLI-arg fallback reason,
/// capture-only — `fly notify --claude --capture`, the `SessionStart` path
/// that updates the pane's resume record and must never ring (KTD1/R2) — or
/// a held permission ask (`fly notify --claude --permission-request`, the
/// `PermissionRequest` path that feeds the ask registry and never rings).
enum HookKind {
    Attention(Reason),
    Capture,
    Ask,
}

impl HookKind {
    /// The `fly notify` invocation this kind installs (quoted absolute binary
    /// path; `--claude` reads the event payload from stdin either way).
    fn command(&self, fly_bin: &Path) -> String {
        match self {
            HookKind::Attention(reason) => {
                format!("\"{}\" notify {} --claude", fly_bin.display(), reason.as_str())
            }
            HookKind::Capture => format!("\"{}\" notify --claude --capture", fly_bin.display()),
            HookKind::Ask => {
                format!("\"{}\" notify --claude --permission-request", fly_bin.display())
            }
        }
    }

    /// The matcher the event's fly group installs, if any. `PermissionRequest`
    /// matches on tool names, so it needs the explicit catch-all (verified on
    /// 2.1.207: `"*"` fires for `Bash` and `AskUserQuestion` alike); the other
    /// events install matcher-less, which fires for every value.
    fn matcher(&self) -> Option<&'static str> {
        match self {
            HookKind::Ask => Some("*"),
            _ => None,
        }
    }
}

/// The Claude Code events fly hooks into, and what each does. v1's only agent;
/// additional agents are deferred (Scope Boundaries). `SessionStart` installs
/// with **no matcher**, so it fires for every source — `startup`, `resume`,
/// `clear`, `compact` — keeping the captured id current across `/clear`
/// rotation (fix-attribution KTD5; all three key sources verified live in U1).
/// `PermissionRequest` (hook-ask-channel U5/R1) fires at ask time for every
/// dialog — including AskUserQuestion, including under bypassPermissions
/// (live-verified 2.1.207) — and holds until the ask resolves; on a Claude too
/// old for the event it simply never fires (detection degrades to the
/// transcript/screen chain).
const CLAUDE_HOOK_EVENTS: &[(&str, HookKind)] = &[
    ("Notification", HookKind::Attention(Reason::Permission)),
    ("Stop", HookKind::Attention(Reason::Finished)),
    ("SessionStart", HookKind::Capture),
    ("PermissionRequest", HookKind::Ask),
];

/// CLI: `fly hooks setup [--agent claude]`.
pub fn run_setup(args: &[String]) -> i32 {
    if let Err(code) = require_claude_agent(args) {
        return code;
    }
    let bin = match canonical_self() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fly hooks setup: cannot resolve fly binary path: {e}");
            return 1;
        }
    };
    let settings = claude_settings_path();
    match apply(&settings, &bin) {
        Ok(()) => {
            println!(
                "fly: installed Claude Code hooks in {} (fly = {})",
                settings.display(),
                bin.display()
            );
            0
        }
        Err(e) => {
            eprintln!("fly hooks setup: {e}");
            1
        }
    }
}

/// CLI: `fly hooks teardown`.
pub fn run_teardown(_args: &[String]) -> i32 {
    let settings = claude_settings_path();
    match teardown(&settings) {
        Ok(()) => {
            println!("fly: removed Claude Code hooks from {}", settings.display());
            0
        }
        Err(e) => {
            eprintln!("fly hooks teardown: {e}");
            1
        }
    }
}

/// Install fly's hooks into `settings_path`, merging with existing content.
pub fn apply(settings_path: &Path, fly_bin: &Path) -> std::io::Result<()> {
    let mut root = read_object(settings_path)?;
    backup_once(settings_path)?;

    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| err("`hooks` is not an object"))?;

    for (event, kind) in CLAUDE_HOOK_EVENTS {
        let command = kind.command(fly_bin);
        let groups = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| err("hook event is not an array"))?;
        // Replace any prior fly group in place (idempotent across re-runs and
        // schema changes); leave the user's own hooks untouched. The fly-group
        // marker keys on the first two command tokens (`fly notify`), so the
        // --capture and --permission-request variants are recognized unchanged
        // (KTD5; hook-ask-channel U5).
        groups.retain(|group| !group_is_fly(group));
        let mut group = json!({
            "hooks": [ { "type": "command", "command": command } ]
        });
        if let Some(matcher) = kind.matcher() {
            group["matcher"] = json!(matcher);
        }
        groups.push(group);
    }

    write_pretty(settings_path, &root)
}

/// Remove only fly's hooks from `settings_path`.
pub fn teardown(settings_path: &Path) -> std::io::Result<()> {
    let mut root = read_object(settings_path)?;
    if let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (event, _) in CLAUDE_HOOK_EVENTS {
            if let Some(groups) = hooks.get_mut(*event).and_then(|g| g.as_array_mut()) {
                groups.retain(|group| !group_is_fly(group));
            }
        }
        // Drop now-empty event arrays, then the hooks object if it's empty.
        hooks.retain(|_, v| !v.as_array().is_some_and(|a| a.is_empty()));
        if hooks.is_empty() {
            root.remove("hooks");
        }
    }
    write_pretty(settings_path, &root)
}

fn require_claude_agent(args: &[String]) -> Result<(), i32> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--agent" {
            match it.next().map(String::as_str) {
                Some("claude") | None => {}
                Some(other) => {
                    eprintln!(
                        "fly hooks setup: only --agent claude is supported in v1 (got {other:?})"
                    );
                    return Err(2);
                }
            }
        }
    }
    Ok(())
}

/// A hook matcher-group belongs to fly if any of its commands invoke `fly notify`.
fn group_is_fly(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_fly_command)
            })
        })
}

/// True if a command string runs `fly notify` (path- and quote-independent).
fn is_fly_command(cmd: &str) -> bool {
    let cleaned = cmd.replace('"', " ");
    let mut parts = cleaned.split_whitespace();
    let bin = parts.next().unwrap_or("");
    let sub = parts.next().unwrap_or("");
    let base = bin.rsplit('/').next().unwrap_or(bin);
    base == "fly" && sub == "notify"
}

fn claude_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("settings.json")
}

fn canonical_self() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn read_object(path: &Path) -> std::io::Result<Map<String, Value>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| err(&format!("{} is not valid JSON: {e}", path.display())))?;
            match value {
                Value::Object(map) => Ok(map),
                _ => Err(err(&format!("{} is not a JSON object", path.display()))),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e),
    }
}

fn backup_once(settings_path: &Path) -> std::io::Result<()> {
    if !settings_path.exists() {
        return Ok(());
    }
    let backup = PathBuf::from(format!("{}.fly.bak", settings_path.display()));
    if !backup.exists() {
        std::fs::copy(settings_path, &backup)?;
    }
    Ok(())
}

fn write_pretty(path: &Path, root: &Map<String, Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root.clone()))?;
    text.push('\n');
    let mut f = std::fs::File::create(path)?;
    f.write_all(text.as_bytes())
}

fn err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, msg.to_string())
}
