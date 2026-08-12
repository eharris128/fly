//! The ported command table (Electron-shell migration U2): every Tauri
//! command that doesn't need the display shell itself, registered under its
//! **exact** Tauri name with its exact camelCase argument keys (KTD1 — the
//! wire shapes are the same ones `src/ipc.ts` already sends). Each entry is
//! the same delegation the `#[tauri::command]` wrapper makes, over the same
//! shared managers, so the two shells cannot diverge in behavior — where a
//! command's body holds real logic (e.g. `pty_write`'s attention clear), the
//! logic lives in a shared fn or manager method used by both.
//!
//! Deliberately **not** here (U3, they need the event/stream plumbing or the
//! shell): `spawn_pane` (byte channel + exit events), `register_alert_sink`
//! (closes over the app handle). They answer with a distinct error so a
//! premature shell integration fails loudly, not mysteriously.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use super::server::CommandHandler;
use crate::automations::AutomationManager;
use crate::config::{Config, ConfigStore};
use crate::feed::FeedState;
use crate::pty::{PaneId, PtyManager};
use crate::state::AttentionManager;
use crate::stream::coalesce::CoalescerRegistry;
use crate::stream::{attention_event_payload, PANE_ATTENTION_EVENT};

/// Where a registry-dispatched command sends events (`pane://…` names, KTD1).
/// `fly core` wires this to `ControlServer::broadcast_event`; tests record.
pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// The shared state a registry closes over — the control-socket counterpart
/// of the Tauri `.manage(…)` set. `automations`/`feed` are optional because
/// the U2 core boots without the automations subsystem (it arrives with U3's
/// full backend host); their commands answer a clear error when absent.
pub struct CoreHandles {
    pub pty: Arc<PtyManager>,
    pub attention: Arc<AttentionManager>,
    pub config: Arc<ConfigStore>,
    pub coalescers: Arc<CoalescerRegistry>,
    pub automations: Option<Arc<AutomationManager>>,
    pub feed: Option<Arc<FeedState>>,
    pub events: EventSink,
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
}

fn to_ok<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| format!("serialize: {e}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaneArg {
    pane_id: PaneId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeafArgs {
    leaf_key: String,
    #[serde(default)]
    live_cwd: Option<String>,
}

/// Build the command handler for a core serving these handles. The returned
/// closure runs on a connection's read thread (`docs/core-protocol.md`).
pub fn build_registry(h: CoreHandles) -> CommandHandler {
    Arc::new(move |cmd, args| {
        match cmd {
            // ---- protocol built-ins -------------------------------------
            "core/ping" => Ok(json!({
                "pong": true,
                "version": env!("CARGO_PKG_VERSION"),
            })),

            // ---- shell/session glue -------------------------------------
            "frontend_log" => {
                #[derive(Deserialize)]
                struct A {
                    msg: String,
                }
                let a: A = parse(args)?;
                // Same line as lib.rs::frontend_log — the webview console is
                // invisible, stderr is the log.
                eprintln!("[fly-webview] {}", a.msg);
                Ok(Value::Null)
            }

            // ---- config -------------------------------------------------
            "get_config" => to_ok(h.config.get()),
            "set_config" => {
                #[derive(Deserialize)]
                struct A {
                    config: Config,
                }
                let a: A = parse(args)?;
                h.config.set(a.config.clone())?;
                to_ok(a.config)
            }

            // ---- pty ----------------------------------------------------
            "pty_write" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_id: PaneId,
                    data: String,
                }
                let a: A = parse(args)?;
                h.pty.write(a.pane_id, a.data.as_bytes())?;
                if let Some(outcome) = h.attention.on_input(a.pane_id) {
                    (h.events)(
                        PANE_ATTENTION_EVENT,
                        attention_event_payload(a.pane_id, &outcome),
                    );
                }
                Ok(Value::Null)
            }
            "pty_resize" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_id: PaneId,
                    rows: u16,
                    cols: u16,
                }
                let a: A = parse(args)?;
                h.pty.resize(a.pane_id, a.rows, a.cols)?;
                Ok(Value::Null)
            }
            "close_pane" => {
                let a: PaneArg = parse(args)?;
                h.pty.close(a.pane_id)?;
                Ok(Value::Null)
            }
            "pty_pause" => {
                let a: PaneArg = parse(args)?;
                h.pty.pause(a.pane_id)?;
                Ok(Value::Null)
            }
            "pty_resume" => {
                let a: PaneArg = parse(args)?;
                h.pty.resume(a.pane_id)?;
                Ok(Value::Null)
            }
            "pane_cwd" => {
                let a: PaneArg = parse(args)?;
                to_ok(h.pty.cwd(a.pane_id).map(|p| p.to_string_lossy().into_owned()))
            }
            "pane_command" => {
                let a: PaneArg = parse(args)?;
                to_ok(h.pty.pane_command(a.pane_id))
            }
            "pane_session_id" => {
                let a: PaneArg = parse(args)?;
                to_ok(h.pty.pane_session_id(a.pane_id))
            }
            "pane_activity" => {
                let a: PaneArg = parse(args)?;
                to_ok(crate::pty::pane_activity_snapshot(&h.pty, a.pane_id))
            }
            "panes_status" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_ids: Vec<PaneId>,
                }
                let a: A = parse(args)?;
                to_ok(h.pty.panes_status(&a.pane_ids))
            }

            // ---- attention / focus replication --------------------------
            "attach_pane" => {
                let a: PaneArg = parse(args)?;
                crate::stream::attach_pane_now(&h.pty, &h.config, a.pane_id)?;
                Ok(Value::Null)
            }
            "set_visible_panes" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_ids: Vec<PaneId>,
                }
                let a: A = parse(args)?;
                let ids: Vec<u64> = a.pane_ids.iter().map(|p| p.0).collect();
                h.coalescers.set_visible_panes(&ids);
                for (pane, outcome) in h.attention.set_visible_panes(&a.pane_ids) {
                    (h.events)(PANE_ATTENTION_EVENT, attention_event_payload(pane, &outcome));
                }
                Ok(Value::Null)
            }
            "set_window_foreground" => {
                #[derive(Deserialize)]
                struct A {
                    foregrounded: bool,
                }
                let a: A = parse(args)?;
                for (pane, outcome) in h.attention.set_foreground(a.foregrounded) {
                    (h.events)(PANE_ATTENTION_EVENT, attention_event_payload(pane, &outcome));
                }
                Ok(Value::Null)
            }
            "set_panel_open" => {
                #[derive(Deserialize)]
                struct A {
                    open: bool,
                }
                let a: A = parse(args)?;
                h.attention.set_panel_open(a.open);
                Ok(Value::Null)
            }
            "set_muted" => {
                #[derive(Deserialize)]
                struct A {
                    muted: bool,
                }
                let a: A = parse(args)?;
                h.attention.set_muted(a.muted);
                Ok(Value::Null)
            }
            "set_workspace_muted" => {
                #[derive(Deserialize)]
                struct A {
                    workspace: String,
                    muted: bool,
                }
                let a: A = parse(args)?;
                h.attention.set_workspace_muted(a.workspace, a.muted);
                Ok(Value::Null)
            }
            "set_pane_workspace" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_id: PaneId,
                    workspace: String,
                }
                let a: A = parse(args)?;
                h.attention.set_pane_workspace(a.pane_id, a.workspace);
                Ok(Value::Null)
            }

            // ---- session / scrollback (plain fns, no managed state) -----
            "save_session" => {
                #[derive(Deserialize)]
                struct A {
                    layout: Value,
                }
                let a: A = parse(args)?;
                crate::session::save_session(a.layout)?;
                Ok(Value::Null)
            }
            "load_session" => to_ok(crate::session::load_session()),
            "save_scrollback" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_key: String,
                    data: String,
                }
                let a: A = parse(args)?;
                crate::session::save_scrollback(a.pane_key, a.data)?;
                Ok(Value::Null)
            }
            "load_scrollback" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    pane_key: String,
                }
                let a: A = parse(args)?;
                to_ok(crate::session::load_scrollback(a.pane_key))
            }

            // ---- resume / attribution -----------------------------------
            "load_resume_records" => to_ok(crate::session::resume::load_resume_records()),
            "save_resume_record" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    leaf_key: String,
                    argv: Vec<String>,
                }
                let a: A = parse(args)?;
                crate::session::resume::save_resume_record(a.leaf_key, a.argv)?;
                Ok(Value::Null)
            }
            "save_resume_session" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    leaf_key: String,
                    session_id: String,
                    #[serde(default)]
                    session_cwd: Option<String>,
                }
                let a: A = parse(args)?;
                to_ok(crate::session::resume::save_resume_session(
                    a.leaf_key,
                    a.session_id,
                    a.session_cwd,
                )?)
            }
            "save_session_pick" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    leaf_key: String,
                    session_id: String,
                    #[serde(default)]
                    session_cwd: Option<String>,
                }
                let a: A = parse(args)?;
                to_ok(crate::session::resume::save_session_pick(
                    a.leaf_key,
                    a.session_id,
                    a.session_cwd,
                )?)
            }
            "reset_pane_attribution" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    leaf_key: String,
                }
                let a: A = parse(args)?;
                crate::session::resume::reset_pane_attribution(a.leaf_key)?;
                Ok(Value::Null)
            }
            "prune_resume_records" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    live_leaf_keys: Vec<String>,
                }
                let a: A = parse(args)?;
                crate::session::resume::prune_resume_records(a.live_leaf_keys)?;
                Ok(Value::Null)
            }

            // ---- transcript / handoff -----------------------------------
            "continue_target" => {
                #[derive(Deserialize)]
                struct A {
                    cwd: String,
                }
                let a: A = parse(args)?;
                to_ok(crate::session::transcript::continue_target(a.cwd))
            }
            "qualifying_session_count" => {
                #[derive(Deserialize)]
                struct A {
                    cwd: String,
                }
                let a: A = parse(args)?;
                to_ok(crate::session::transcript::qualifying_session_count(a.cwd))
            }
            "resolve_resume_spawn_cwd" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    session_id: String,
                    recorded_cwd: String,
                }
                let a: A = parse(args)?;
                to_ok(crate::session::transcript::resolve_resume_spawn_cwd(
                    a.session_id,
                    a.recorded_cwd,
                ))
            }
            "resolve_handoff_target" => {
                let a: LeafArgs = parse(args)?;
                to_ok(crate::session::handoff::resolve_handoff_target(
                    a.leaf_key, a.live_cwd,
                ))
            }
            "list_handoff_candidates" => {
                let a: LeafArgs = parse(args)?;
                to_ok(crate::session::handoff::list_handoff_candidates(
                    a.leaf_key, a.live_cwd,
                ))
            }

            // ---- usage --------------------------------------------------
            // Async under Tauri; here a small current-thread runtime blocks
            // the connection thread for the fetch's own short timeout — the
            // dashboard-open one-shot (KTD-C), not a hot path.
            "usage_snapshot" => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("usage runtime: {e}"))?;
                to_ok(rt.block_on(crate::usage::usage_snapshot())?)
            }

            // ---- automations / feed (absent until the U3 full host) -----
            "automations_frontend_ready" => {
                let m = h.automations.as_ref().ok_or("automations unavailable in this core (U3)")?;
                m.set_frontend_ready();
                Ok(Value::Null)
            }
            "list_automations" => {
                let m = h.automations.as_ref().ok_or("automations unavailable in this core (U3)")?;
                to_ok(crate::automations::dashboard_snapshot(m))
            }
            "delete_automation" => {
                #[derive(Deserialize)]
                struct A {
                    id: String,
                }
                let m = h.automations.as_ref().ok_or("automations unavailable in this core (U3)")?;
                let a: A = parse(args)?;
                m.delete(&a.id).map(|_| ())?;
                Ok(Value::Null)
            }
            "monitor_pickup_check" => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct A {
                    transcript_path: String,
                    cwd: String,
                }
                let a: A = parse(args)?;
                to_ok(crate::automations::monitor_pickup_check(a.transcript_path, a.cwd))
            }
            "read_monitor_bundle" => {
                #[derive(Deserialize)]
                struct A {
                    path: String,
                }
                let m = h.automations.as_ref().ok_or("automations unavailable in this core (U3)")?;
                let a: A = parse(args)?;
                to_ok(crate::automations::read_bundle_for(m, &a.path)?)
            }
            "publish_agent_feed" => {
                #[derive(Deserialize)]
                struct A {
                    payload: crate::feed::wire::FeedPublishPayload,
                }
                let f = h.feed.as_ref().ok_or("feed unavailable in this core (U3)")?;
                let a: A = parse(args)?;
                to_ok(f.publish(a.payload.agents, crate::feed::now_ms()))
            }

            // ---- shell-coupled: arrive with U3 --------------------------
            "spawn_pane" | "register_alert_sink" | "get_launch_mode" => Err(format!(
                "{cmd} is not available over the control socket yet (U3)"
            )),

            other => Err(format!("unknown command: {other}")),
        }
    })
}
