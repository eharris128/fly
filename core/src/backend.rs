//! The backend builder (Electron-shell migration U3.5).
//!
//! Everything the backend is — the hook server's dispatch (attention →
//! policy → notify, resume capture, feed bumps), the ask/peer/automation/
//! substrate handlers, the automations manager + sweep + alert surfacing,
//! and the feed listener — is constructed HERE, against two injected seams:
//!
//! - [`BackendSeams::events`] — where `pane://…` / `automation://…` /
//!   `notification://…` events land (control-socket broadcast under `fly
//!   core`; a recorder in tests);
//! - [`BackendSeams::banner`] — the desktop-notification surface
//!   (`notify::banner` under `fly core`; a no-op in tests).
//!
//! `fly core` calls [`build_backend`] and serves the pieces over the control
//! socket; `tests/backend_build.rs` boots the same thing headless. One
//! constructor, so the wiring cannot drift (KTD1/KTD2). Change backend
//! wiring here, never by inlining into a host.

use std::sync::Arc;

use crate::automations;
use crate::config::ConfigStore;
use crate::feed;
use crate::hooks::{self, HookServer, TokenRegistry, ValidatedHook};
use crate::notify;
use crate::peer;
use crate::pty::{PaneId, PtyManager};
use crate::session;
use crate::state::attention::{Reason, Signal, Tier};
use crate::notify::{NotificationGate, Surfaced};
use crate::state::AttentionManager;
use crate::stream;
use crate::substrate;
use crate::usage;

/// The two shell-owned surfaces the backend needs injected.
pub struct BackendSeams {
    pub events: stream::EventSink,
    pub banner: Arc<dyn Fn(&str, &str) + Send + Sync>,
}

/// Everything a shell needs to serve: the managers commands dispatch over,
/// the running servers, and the sweep handle for ordered shutdown.
pub struct Backend {
    pub config: Arc<ConfigStore>,
    pub pty: Arc<PtyManager>,
    pub tokens: Arc<TokenRegistry>,
    pub attention: Arc<AttentionManager>,
    pub coalescers: Arc<stream::coalesce::CoalescerRegistry>,
    pub substrate: Option<Arc<substrate::Substrate>>,
    pub pending_signals: Arc<feed::pending::PendingSignals>,
    pub ask_registry: Arc<feed::ask::AskRegistry>,
    pub hook_server: HookServer,
    pub alerts: Arc<automations::alerts::AlertsLog>,
    pub script_runner: Arc<automations::script::ScriptRunner>,
    pub automations: Arc<automations::AutomationManager>,
    pub sweep: automations::SweepHandle,
    pub feed_state: Arc<feed::FeedState>,
    pub feed_server: Option<feed::FeedServer>,
}

impl Backend {
    /// Ordered shutdown for a shell-less host (Electron-shell migration U6):
    /// the ordered sequence over this backend's own pieces. `fly core` runs
    /// it on `core/shutdown` or SIGTERM/SIGINT, so an Electron quit tears
    /// down in order — clean-exit marker written, in-flight runs closed,
    /// substrate sessions detached (not killed).
    pub fn shutdown(&self) {
        ordered_shutdown(
            Some(&self.sweep),
            Some(&self.automations),
            Some(&self.feed_state),
            Some(&self.ask_registry),
            Some(&self.pty),
        );
    }
}

/// The one ordered teardown (U6; the sequence and rationale moved verbatim
/// from the original `lifecycle.rs`). Every piece is optional so an
/// early-exit boot that never constructed them all can still run it.
///
/// Order (each step's reason, from the original lifecycle doc):
/// 1. Clean-exit marker BEFORE reaping (KTD-G): reaching this ordered path at
///    all means the quit was clean; an unclean exit never runs this, leaving
///    the marker absent → the next launch offers resume. Best-effort.
/// 2. Automations (R5) BEFORE the PTY reap: stop+join the sweep (no store
///    lock held — KTD-B; once joined no new claim races the closes), kill
///    in-flight script groups and headless children, close every in-flight
///    row failed("interrupted") — a run row must never record a pane exit as
///    the outcome of a shutdown.
/// 3. Feed: wake every blocked SSE reader so listener threads exit promptly,
///    before the panes they describe are reaped.
/// 4. Held asks (hook-ask-channel R9): release every held hook connection so
///    no hook process is left holding a socket into teardown.
/// 5. PTY reap last: close every pane — tmux-backed panes DETACH (sessions
///    outlive fly), ephemeral and plain-PTY panes reap; no zombies/orphans.
pub fn ordered_shutdown(
    sweep: Option<&automations::SweepHandle>,
    automations_mgr: Option<&Arc<automations::AutomationManager>>,
    feed_state: Option<&Arc<feed::FeedState>>,
    asks: Option<&Arc<feed::ask::AskRegistry>>,
    pty: Option<&Arc<PtyManager>>,
) {
    let _ = crate::session::resume::set_clean_exit_at(
        &crate::session::resume::clean_exit_path(),
        true,
    );
    if let Some(sweep) = sweep {
        sweep.stop_and_join();
    }
    if let Some(mgr) = automations_mgr {
        mgr.shutdown();
    }
    if let Some(feed) = feed_state {
        feed.shutdown();
    }
    if let Some(asks) = asks {
        asks.shutdown();
    }
    if let Some(pty) = pty {
        pty.close_all();
    }
}

/// U6 event payload: an alert arrived with no sink pane registered (R17). The
/// frontend single-flights a background "Automations" tab that `tail -f`s
/// `log_path`, then calls `register_alert_sink` with the new pane id.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AlertPendingEvent {
    log_path: String,
}

/// Build the full backend. The construction order is deliberate: state cores
/// first, then the automations subsystem, then the hook server's dispatch
/// (which closes over direct Arcs — the old lazy `try_state` resolution was
/// only a construction-order workaround), then the feed listener.
pub fn build_backend(seams: BackendSeams) -> Result<Backend, String> {
    let BackendSeams { events, banner } = seams;

    let config = Arc::new(ConfigStore::load(crate::config::default_path()));
    // Immutable snapshot of settings (no runtime config reload in v1).
    let cfg = config.get();
    let pty = Arc::new(PtyManager::new());
    // KTD10 (tmux-substrate U3): when the rollout flag selects tmux, every
    // leaf-keyed spawn becomes a marked session on the flavor server.
    let substrate_handle = if cfg.substrate == crate::config::SubstrateKind::Tmux {
        let runtime_dir = crate::hook_socket_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let sub = Arc::new(substrate::Substrate::new(
            crate::app_dir_name(),
            session::data_dir().join("substrate-sessions.json"),
            runtime_dir,
            crate::hook_socket_path(),
            std::env::current_exe().unwrap_or_else(|_| "fly".into()),
        ));
        pty.set_substrate(Arc::clone(&sub));
        Some(sub)
    } else {
        None
    };
    let tokens = Arc::new(TokenRegistry::new());
    let attention = Arc::new(AttentionManager::new(
        cfg.attention_debounce_ms,
        cfg.notifications_muted_default,
    ));
    let coalescers = Arc::new(stream::coalesce::CoalescerRegistry::default());
    let gate = Arc::new(NotificationGate::new(cfg.notification_coalesce_threshold));
    // Ask-time raise stamps (feed-question-screen-fallback U3/KTD5). Created
    // unconditionally (like FeedState) so stamping never depends on the feed
    // listener being enabled.
    let pending_signals = Arc::new(feed::pending::PendingSignals::new());
    // Held permission asks (hook-ask-channel U8) — registration works whether
    // or not the feed listener is enabled; shutdown releases held hooks (R9).
    let ask_registry = Arc::new(feed::ask::AskRegistry::new());
    // FeedState exists unconditionally: the pushed roster is cached even when
    // the listener never starts, and the dispatch below bumps it directly.
    let feed_state = Arc::new(feed::FeedState::new());

    // ---- automations subsystem (U4–U7 of its plan) --------------------------
    // Manager first (with the unwired placeholder dispatcher), runners next,
    // seams injected post-construction exactly as before. The changed-emitter
    // both fans the event out and bumps the feed (one hop, no listener
    // bridge).
    let changed_events = Arc::clone(&events);
    let feed_for_changed = Arc::clone(&feed_state);
    let automations_mgr = Arc::new(automations::AutomationManager::new(
        automations::store::Store::load_default(),
        Arc::new(automations::UnwiredDispatcher),
        Box::new(notify::now_unix_ms),
        Box::new(move |id: &str| {
            changed_events(
                automations::AUTOMATION_CHANGED_EVENT,
                serde_json::Value::String(id.to_string()),
            );
            feed_for_changed.bump();
        }),
    ));
    // Agent dispatcher (U7): emits automation://agent-run for the frontend to
    // spawn the paned run (reachable only via --paned/knob).
    let agent_dispatcher = automations::AgentDispatcher::new(Arc::clone(&events));
    // Script runner (U5): the real dispatcher for script-mode runs; the Weak
    // breaks the manager↔runner Arc cycle.
    let closer_mgr = Arc::downgrade(&automations_mgr);
    let script_runner = Arc::new(automations::script::ScriptRunner::new(Arc::new(
        move |automation_id: &str, run_id: &str, outcome: automations::model::RunOutcome| {
            if let Some(mgr) = closer_mgr.upgrade() {
                let _ = mgr.close_run(automation_id, run_id, outcome);
            }
        },
    )));
    // Headless monitor-check runner (headless-monitor-checks U3/U5).
    let headless_closer_mgr = Arc::downgrade(&automations_mgr);
    let headless_runner = Arc::new(automations::headless::HeadlessRunner::new(Arc::new(
        move |automation_id: &str, run_id: &str, outcome: automations::headless::CheckOutcome| {
            if let Some(mgr) = headless_closer_mgr.upgrade() {
                mgr.close_headless_run(automation_id, run_id, outcome);
            }
        },
    )));

    // U6 alert surfacing (R16/R17/R18): append the sanitized line, then ring
    // the registered sink pane or queue + ask the frontend to open one.
    let alerts_log = automations::alerts::AlertsLog::open_default();
    let surface_alert: Arc<dyn Fn(&str, &str) + Send + Sync> = {
        let log = Arc::clone(&alerts_log);
        let attention = Arc::clone(&attention);
        let events = Arc::clone(&events);
        Arc::new(move |name: &str, first_line: &str| {
            log.append(name, first_line); // R16
            let queued = automations::alerts::QueuedAlert {
                automation_name: name.to_string(),
                first_line: first_line.to_string(),
            };
            match log.sink_or_queue(queued) {
                // R18: a live sink pane rings via the attention pipeline.
                Some(pane_id) => crate::raise_alert_with(&events, &attention, pane_id),
                // R17: no sink yet — queued; ask the frontend to open the
                // sink pane, which registers and drains the backlog.
                None => {
                    events(
                        "automation://alert-pending",
                        serde_json::to_value(AlertPendingEvent {
                            log_path: log.log_path().to_string_lossy().into_owned(),
                        })
                        .expect("alert event serializes"),
                    );
                }
            }
        })
    };
    let script_alert = Arc::clone(&surface_alert);
    script_runner.set_alert_sink(Arc::new(move |ev: automations::script::AlertEvent| {
        script_alert(&ev.automation_name, &ev.first_line);
    }));
    // Interrupt-resilience alert sink (interrupt-resilience U3).
    let interrupt_alert = Arc::clone(&surface_alert);
    automations_mgr.set_interrupt_sink(Arc::new(move |ir: &automations::InterruptedRun| {
        let line = if ir.is_retry_eligible() {
            "interrupted by restart — retrying"
        } else {
            "interrupted by restart"
        };
        interrupt_alert(&ir.name, line);
    }));
    // Monitor verdict / broken-monitor alerts (monitor-handoff U3).
    automations_mgr.set_monitor_alert_sink(Arc::clone(&surface_alert));
    automations_mgr.set_bundle_dir(session::data_dir().join("monitor-bundles"));
    automations_mgr.set_dispatcher(Arc::new(CompositeDispatcher {
        agent: agent_dispatcher,
        script: Arc::clone(&script_runner),
        headless: Arc::clone(&headless_runner),
    }));
    // U4a: agent-launch resolution reads the user's shared automation defaults.
    automations_mgr.set_config(Arc::clone(&config));
    // U4b: capture an agent run's final assistant turn on close (R8) —
    // bounded retry (Stop precedes the transcript flush), then the ONE shared
    // sanitize→scrub composition.
    automations_mgr.set_output_capturer(Arc::new(|cwd: &str, dispatched_ms: u64| {
        let text = session::transcript::capture_final_assistant_since(
            std::path::Path::new(cwd),
            dispatched_ms,
            session::transcript::CAPTURE_ATTEMPTS,
            session::transcript::CAPTURE_RETRY_DELAY,
        )?;
        automations::redact::clean_captured(&text)
    }));
    // U5: forward agent-run closes so the tab lifecycle (U8) can react.
    let run_closed_events = Arc::clone(&events);
    automations_mgr.set_run_closed_emitter(Arc::new(move |ev: &automations::RunClosedEvent| {
        if let Ok(payload) = serde_json::to_value(ev) {
            run_closed_events("automation://run-closed", payload);
        }
    }));
    // monitor-handoff U4 (R13): tell the frontend which pane registered.
    let monitor_registered_events = Arc::clone(&events);
    automations_mgr.set_monitor_registered_emitter(Arc::new(
        move |ev: &automations::MonitorRegisteredEvent| {
            if let Ok(payload) = serde_json::to_value(ev) {
                monitor_registered_events(automations::MONITOR_REGISTERED_EVENT, payload);
            }
        },
    ));
    let killer_runner = Arc::clone(&script_runner);
    automations_mgr.set_script_killer(Arc::new(move |run_id: &str| killer_runner.kill_run(run_id)));
    let capacity_runner = Arc::clone(&script_runner);
    automations_mgr.set_script_capacity(Arc::new(move || capacity_runner.has_capacity()));
    // Headless seams (headless-monitor-checks U4/U5 — R5/R7).
    let headless_killer = Arc::clone(&headless_runner);
    automations_mgr
        .set_headless_killer(Arc::new(move |run_id: &str| headless_killer.kill_run(run_id)));
    let headless_alive = Arc::clone(&headless_runner);
    automations_mgr.set_headless_check_alive(Arc::new(move |automation_id: &str| {
        headless_alive.automation_check_alive(automation_id)
    }));
    let headless_gate = Arc::clone(&headless_runner);
    automations_mgr.set_headless_deadline_gate(Arc::new(move |run_id: &str| {
        headless_gate.monotonic_deadline_lapsed(run_id)
    }));
    // U7 pane-alive probe (KTD-D): only a *live* pane counts.
    let pty_for_alive = Arc::clone(&pty);
    automations_mgr.set_agent_pane_alive(Arc::new(move |row: &automations::model::RunRow| {
        row.pane_id
            .and_then(|id| pty_for_alive.lifecycle(PaneId(id)))
            .is_some_and(|s| s.is_live())
    }));
    // Usage-limit-deferral U4: the plan-usage gate (fail-open, KTD3).
    let usage_gate = Arc::new(usage::gate::OauthUsageGate::new());
    automations_mgr.set_usage_gate(Arc::new(move |now_ms: u64| usage_gate.defer_floor(now_ms)));

    // ---- the hook dispatch (the attention pipeline's step 4/5) --------------
    // Verbatim behavior from the original inline closure; the pieces are
    // direct Arcs now that construction order allows it.
    let dispatch: hooks::Dispatch = {
        let attention = Arc::clone(&attention);
        let gate = Arc::clone(&gate);
        let config_snapshot = cfg.clone();
        let pty = Arc::clone(&pty);
        let signals = Arc::clone(&pending_signals);
        let automations_mgr = Arc::clone(&automations_mgr);
        let feed_state = Arc::clone(&feed_state);
        let events = Arc::clone(&events);
        let banner = Arc::clone(&banner);
        Arc::new(move |pane, hook: ValidatedHook| {
            // Resume capture (U3, KTD-A): a session_id-bearing hook upserts the
            // pane's resume record, stamped `Hook` HERE at the call site
            // (fix-session-pane-attribution KTD2).
            if let Some(session_id) = hook.session_id.clone() {
                if let Some(leaf_key) = pty.leaf_key(pane) {
                    let _ = session::resume::upsert_at(
                        &session::resume::resume_path(),
                        &leaf_key,
                        session::resume::ResumePartial {
                            session_id: Some(session_id),
                            session_cwd: hook.cwd.clone(),
                            session_source: Some(session::resume::SessionSource::Hook),
                            ..Default::default()
                        },
                    );
                }
            }

            // Capture-only short-circuit (fix-attribution U2, KTD1/R2).
            if hook.is_capture_only() {
                return;
            }

            // U7 agent-run closure (KTD-F): a Stop on a run-linked pane closes
            // it succeeded — off this thread (Claude blocks on the hook and
            // the U4b capture retries ~2s).
            if hook.hook_event.as_deref() == Some("Stop") {
                {
                    let mgr = Arc::clone(&automations_mgr);
                    let pane_id = pane.0;
                    std::thread::spawn(move || {
                        let _ = mgr.close_run_by_pane(pane_id);
                    });
                }
                // Feed settle bump (feed-agent-reply-io U6/KTD4): the final
                // turn flushes ~100ms after this hook; re-emit after it lands.
                let feed = Arc::clone(&feed_state);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    feed.bump();
                });
            }

            let reason = hook.reason;
            // U8 (KTD5): suppress a background automation pane's *normal
            // completion* raise — its surface is the dashboard.
            if reason == Reason::Finished && automations_mgr.is_automation_pane(pane.0) {
                return;
            }

            // Feed settle bump for Notification (feed-pending-question
            // U5/KTD5): the permission tool_use may not have flushed yet.
            if hook.hook_event.as_deref() == Some("Notification") {
                let feed = Arc::clone(&feed_state);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    feed.bump();
                });
            }

            let signal = Signal {
                reason,
                tier: Tier::Hook,
            };
            let Some(outcome) = attention.signal(pane, signal) else {
                return;
            };
            events(
                stream::PANE_ATTENTION_EVENT,
                stream::attention_event_payload(pane, &outcome),
            );

            // Only a fresh (non-debounced) raise surfaces; duplicates drop.
            if !outcome.recordable {
                return;
            }

            // Ask-time stamp (feed-question-screen-fallback U3/KTD5).
            if matches!(reason, Reason::Question | Reason::Permission) {
                if let Some(leaf_key) = pty.leaf_key(pane) {
                    signals.stamp(&leaf_key, notify::now_unix_ms());
                }
            }

            let reason_effects = config_snapshot.reason_effects.for_reason(reason);
            let Some(decision) = attention.decide(pane, reason_effects) else {
                return;
            };
            let effects = decision.effects;

            // History is decoupled from the banner gate (KTD16).
            if effects.record {
                events(
                    stream::NOTIFICATION_ADDED_EVENT,
                    stream::notification_added_payload(
                        gate.next_id(),
                        pane,
                        reason,
                        hook.title.as_deref().map(notify::sanitize_title),
                        hook.body.as_deref().map(notify::sanitize_body),
                        notify::now_unix_ms(),
                        decision.read,
                    ),
                );
            }

            // Desktop banner: away-only, coalesced + rate-limited.
            let banner_title = hook.title.as_deref().unwrap_or("fly: an agent needs you");
            let banner_body = hook.body.as_deref().unwrap_or("");
            let gate_verdict = if effects.desktop {
                gate.decide(attention.raised_count(), banner_title, banner_body, gate.now_ms())
            } else {
                Surfaced::Suppressed
            };
            let actions = notify::surface_actions(effects, &gate_verdict);
            if actions.banner {
                match &gate_verdict {
                    Surfaced::Individual { title, body } => banner(title, body),
                    Surfaced::Coalesced { count } => {
                        // "Panes", not "agents": alert raises count too.
                        banner("fly", &notify::coalesced_body(*count))
                    }
                    Surfaced::Suppressed => {}
                }
            }
            // Chime follows effects.sound, independent of the banner.
            if effects.sound {
                if let Some(name) = config_snapshot.notification_sound.as_deref() {
                    notify::play_sound(name);
                }
            }
            // Opt-in notification command, gated on the *fired* banner.
            if actions.command {
                if let Some(cmd) = config_snapshot.notification_command.as_deref() {
                    notify::command::run(
                        cmd,
                        banner_title,
                        notify::reason_subtitle(reason),
                        banner_body,
                    );
                }
            }
        })
    };

    // U9 automation request handler: `automation/*` socket ops.
    let automation_handler: hooks::RequestHandler = {
        let mgr = Arc::clone(&automations_mgr);
        let attention = Arc::clone(&attention);
        let pty = Arc::clone(&pty);
        Arc::new(move |pane, buf: &[u8]| {
            handle_automation_request(&mgr, &attention, &pty, pane, buf).to_bytes()
        })
    };
    // Held-ask registration (hook-ask-channel U8).
    let ask_handler: hooks::AskHandler = {
        let pty = Arc::clone(&pty);
        let registry = Arc::clone(&ask_registry);
        let feed_state = Arc::clone(&feed_state);
        Arc::new(move |pane, payload| {
            let leaf_key = pty.leaf_key(pane)?;
            if let Some(session_id) = payload.session_id.clone() {
                let _ = session::resume::upsert_at(
                    &session::resume::resume_path(),
                    &leaf_key,
                    session::resume::ResumePartial {
                        session_id: Some(session_id),
                        session_cwd: payload.cwd.clone(),
                        session_source: Some(session::resume::SessionSource::Hook),
                        ..Default::default()
                    },
                );
            }
            let (generation, rx) = registry.register(&leaf_key, payload, notify::now_unix_ms())?;
            feed_state.bump();
            let drop_registry = Arc::clone(&registry);
            let drop_feed = Arc::clone(&feed_state);
            let on_drop = Box::new(move || {
                if drop_registry.clear_if(&leaf_key, generation) {
                    drop_feed.bump();
                }
            });
            Some(hooks::AskTicket {
                decision_rx: rx,
                on_drop,
            })
        })
    };
    // Peer messaging (agent-peer-messaging U2/U6).
    let peer_handler: hooks::PeerHandler = {
        let pty = Arc::clone(&pty);
        let feed_state = Arc::clone(&feed_state);
        let screen_pty = Arc::clone(&pty);
        let screen: feed::fallback::ScreenFn =
            Arc::new(move |leaf| screen_pty.screen_tail_by_leaf(leaf));
        let asks = Arc::clone(&ask_registry);
        let ask_fn: feed::fallback::AskFn = Arc::new(move |leaf| asks.get(leaf));
        let resolver = Arc::new(feed::fallback::FallbackResolver::new(
            session::resume::resume_path(),
            Arc::clone(&pending_signals),
            screen,
            ask_fn,
        ));
        let buckets = Arc::new(std::sync::Mutex::new(peer::rate::Buckets::new()));
        let delivery_lock = Arc::new(std::sync::Mutex::new(()));
        let attention = Arc::clone(&attention);
        let events = Arc::clone(&events);
        Arc::new(move |pane, buf: &[u8]| {
            handle_peer_request(
                &feed_state,
                &attention,
                &events,
                pane,
                buf,
                &pty,
                &resolver,
                &buckets,
                &delivery_lock,
            )
            .to_bytes()
        })
    };
    // KTD12: substrate event reports from tmux run-shell hooks.
    let substrate_event_handler: Option<hooks::SubstrateHandler> =
        substrate_handle.as_ref().map(|sub| {
            let sub = Arc::clone(sub);
            let pty = Arc::clone(&pty);
            let attention = Arc::clone(&attention);
            let events = Arc::clone(&events);
            let handler: hooks::SubstrateHandler = Arc::new(move |buf: &[u8]| {
                let Ok(ev) = serde_json::from_slice::<hooks::protocol::SubstrateEvent>(buf) else {
                    return false;
                };
                if !sub.validate_event_token(&ev.token) {
                    return false;
                }
                if substrate::validate_session_name(&ev.session).is_err() {
                    return true; // authenticated but malformed → drop
                }
                if ev.kind == "pane-died" {
                    // Hints (KTD12): confirm against tmux before acting.
                    if let Ok(Some(status)) = sub.tmux().pane_dead(&ev.session) {
                        pty.force_dead_by_session(&ev.session, status);
                    }
                } else if ev.kind == "attach-state" {
                    // U7/R9: attached-elsewhere = focused-elsewhere.
                    if let Ok(attached) = sub.tmux().is_session_attached(&ev.session) {
                        if let Some(pane) = pty.pane_id_by_session(&ev.session) {
                            if let Some(o) = attention.set_attached(pane, attached) {
                                events(
                                    stream::PANE_ATTENTION_EVENT,
                                    stream::attention_event_payload(pane, &o),
                                );
                            }
                            events(
                                "pane://attach",
                                serde_json::json!({
                                    "paneId": pane.0,
                                    "attached": attached,
                                }),
                            );
                        }
                    }
                }
                true
            });
            handler
        });

    let hook_server = HookServer::start_all(
        crate::hook_socket_path(),
        Arc::clone(&tokens),
        dispatch,
        Some(automation_handler),
        Some(ask_handler),
        Some(peer_handler),
        substrate_event_handler,
    )
    .map_err(|e| format!("failed to start hook server: {e}"))?;

    // The sweep starts unconditionally (R5); agent dispatch waits for the
    // automations_frontend_ready command.
    let sweep = automations::start_sweep(Arc::clone(&automations_mgr))
        .map_err(|e| format!("failed to start automation sweep: {e}"))?;

    // ---- the feed listener (feat-agent-state-local-feed U6 + successors) ----
    let feed_server = start_feed_if_enabled(
        &config,
        &pty,
        &attention,
        &events,
        &feed_state,
        &ask_registry,
        &pending_signals,
        &automations_mgr,
    );

    Ok(Backend {
        config,
        pty,
        tokens,
        attention,
        coalescers,
        substrate: substrate_handle,
        pending_signals,
        ask_registry,
        hook_server,
        alerts: alerts_log,
        script_runner,
        automations: automations_mgr,
        sweep,
        feed_state,
        feed_server,
    })
}

/// Start the loopback feed listener when enabled + a bearer token exists.
/// A bind failure is non-fatal (never block launch): log and continue.
#[allow(clippy::too_many_arguments)]
fn start_feed_if_enabled(
    config_store: &Arc<ConfigStore>,
    pty: &Arc<PtyManager>,
    attention: &Arc<AttentionManager>,
    events: &stream::EventSink,
    feed_state: &Arc<feed::FeedState>,
    ask_registry: &Arc<feed::ask::AskRegistry>,
    pending_signals: &Arc<feed::pending::PendingSignals>,
    automations_mgr: &Arc<automations::AutomationManager>,
) -> Option<feed::FeedServer> {
    let feed_cfg = config_store.get().feed;
    if !feed_cfg.enabled {
        return None;
    }
    let Some(token) = crate::config::ensure_feed_token(config_store) else {
        eprintln!("[fly] feed server disabled: could not persist bearer token");
        return None;
    };
    // Automations half of the snapshot: projected from the authoritative
    // store at emit time (KTD4). The effective `headless` bit reads the
    // config default per projection (headless-agent-automations U4, R11).
    let mgr_for_feed = Arc::clone(automations_mgr);
    let cfg_for_feed = Arc::clone(config_store);
    let automations_fn: feed::server::AutomationsFn = Arc::new(move || {
        let headless_default = cfg_for_feed.get().automation_defaults.headless;
        mgr_for_feed
            .list()
            .iter()
            .map(|a| feed::wire::AutomationEntry::from_automation(a, headless_default))
            .collect()
    });
    let now_fn: feed::server::NowFn = Arc::new(notify::now_unix_ms);
    // Per-leaf IO resolver: transcript primary, held ask ahead of it, screen
    // fallback behind (feed-question-screen-fallback U5).
    let pty_for_screen = Arc::clone(pty);
    let screen_fn: feed::fallback::ScreenFn =
        Arc::new(move |leaf_key| pty_for_screen.screen_tail_by_leaf(leaf_key));
    let ask_registry_for_io = Arc::clone(ask_registry);
    let ask_fn: feed::fallback::AskFn = Arc::new(move |leaf_key| ask_registry_for_io.get(leaf_key));
    let resolver = Arc::new(feed::fallback::FallbackResolver::new(
        session::resume::resume_path(),
        Arc::clone(pending_signals),
        screen_fn,
        ask_fn,
    ));
    let io_fn: feed::server::IoFn =
        Arc::new(move |leaf_key, reason, status| resolver.resolve_io(leaf_key, reason, status));
    // Input delivery (U5; feed-pending-question U6; hook-ask-channel U8).
    let input_fn: feed::server::InputFn = {
        let pty = Arc::clone(pty);
        let attention = Arc::clone(attention);
        let events = Arc::clone(events);
        let asks_for_input = Arc::clone(ask_registry);
        let feed_state = Arc::clone(feed_state);
        Arc::new(move |leaf_key, action| {
            let Some(pane) = pty.pane_by_leaf(leaf_key) else {
                return feed::server::InputOutcome::UnknownPane;
            };
            // Decision delivery (hook-ask-channel U8/KTD5): through the held
            // connection, never the PTY.
            if let feed::server::InputAction::Decision { allow, if_asked_at } = &action {
                return match asks_for_input.answer(leaf_key, *if_asked_at, *allow) {
                    feed::ask::AnswerOutcome::Delivered => {
                        if let Some(outcome) = attention.on_input(pane) {
                            events(
                                stream::PANE_ATTENTION_EVENT,
                                stream::attention_event_payload(pane, &outcome),
                            );
                        }
                        feed_state.bump();
                        feed::server::InputOutcome::Delivered
                    }
                    feed::ask::AnswerOutcome::Gone => feed::server::InputOutcome::Conflict,
                };
            }
            let delivered = match &action {
                feed::server::InputAction::Submit(text) => pty
                    .write(pane, &feed::io::paste_payload(text))
                    .and_then(|()| {
                        std::thread::sleep(feed::io::SUBMIT_DELAY);
                        pty.write(pane, feed::io::SUBMIT)
                    }),
                feed::server::InputAction::Keys(bytes) => pty.write(pane, bytes),
                // Other answer (feed-other-answer KTD1): digit → text → Enter
                // as three separate, delay-spaced chunks.
                feed::server::InputAction::Other { select, text } => pty
                    .write(pane, select)
                    .and_then(|()| {
                        std::thread::sleep(feed::io::SUBMIT_DELAY);
                        pty.write(pane, text)
                    })
                    .and_then(|()| {
                        std::thread::sleep(feed::io::SUBMIT_DELAY);
                        pty.write(pane, feed::io::SUBMIT)
                    }),
                feed::server::InputAction::Decision { .. } => unreachable!(),
            };
            match delivered {
                Ok(()) => {
                    if let Some(outcome) = attention.on_input(pane) {
                        events(
                            stream::PANE_ATTENTION_EVENT,
                            stream::attention_event_payload(pane, &outcome),
                        );
                    }
                    feed::server::InputOutcome::Delivered
                }
                Err(e) => feed::server::InputOutcome::Failed(e),
            }
        })
    };
    // Phone-drop delivery (phone-screenshot-drop U5/U6).
    let drop_fn: feed::server::DropFn = {
        let pty = Arc::clone(pty);
        let attention = Arc::clone(attention);
        let events = Arc::clone(events);
        Arc::new(move |leaf_key, delivery| {
            let outcome = feed::drop::deliver_with_guards_verified(
                delivery.expect_pane,
                delivery.text,
                || pty.pane_by_leaf(leaf_key).map(|p| p.0),
                |pane| pty.is_agent(PaneId(pane)),
                |pane, bytes| pty.write(PaneId(pane), bytes),
                || std::thread::sleep(feed::io::SUBMIT_DELAY),
                delivery.commit,
                |pane| pty.wake_if_detached(PaneId(pane)),
                |pane| pty.pane_output_seq(PaneId(pane)),
                std::thread::sleep,
            );
            if matches!(outcome, feed::drop::DropOutcome::Delivered) {
                if let Some(pane) = pty.pane_by_leaf(leaf_key) {
                    if let Some(o) = attention.on_input(pane) {
                        events(
                            stream::PANE_ATTENTION_EVENT,
                            stream::attention_event_payload(pane, &o),
                        );
                    }
                }
            }
            outcome
        })
    };
    let drop_store = {
        let raw = feed_cfg.drop_dir.clone();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        match feed::drop::resolve_drop_dir(raw.as_deref(), home.as_deref(), &session::data_dir()) {
            Ok(dir) => match feed::drop::DropStore::new(&dir) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    log::warn!("phone drop directory {} unusable: {e}", dir.display());
                    None
                }
            },
            Err(e) => {
                log::warn!("feed.dropDir is not usable: {e}");
                None
            }
        }
    };
    let drop_cfg = feed::server::DropConfig {
        deliver: drop_fn,
        store: drop_store,
        max_bytes: u64::from(feed_cfg.drop_max_bytes),
        expected_tailnet_login: feed_cfg.expected_tailnet_login.clone(),
    };
    let permission_answers_fn: feed::server::PermissionAnswersFn = {
        let cfg = Arc::clone(config_store);
        Arc::new(move || cfg.get().feed.allow_permission_answers)
    };
    match feed::FeedServer::start(
        feed_cfg.port,
        token,
        Arc::clone(feed_state),
        automations_fn,
        now_fn,
        io_fn,
        input_fn,
        drop_cfg,
        permission_answers_fn,
    ) {
        Ok(server) => Some(server),
        Err(e) => {
            eprintln!("[fly] feed server disabled: bind failed: {e}");
            None
        }
    }
}

/// Automation-op entry (U9): parse, gate, dispatch — the socket counterpart
/// of the dashboard's mutations. Arc-parameterized (U3.5): both shells route
/// the hook server here.
fn handle_automation_request(
    mgr: &Arc<automations::AutomationManager>,
    attention: &Arc<AttentionManager>,
    pty: &Arc<PtyManager>,
    pane: PaneId,
    buf: &[u8],
) -> crate::cli::automation::AutomationResponse {
    use crate::cli::automation::{AutomationRequest, AutomationResponse};

    let req: AutomationRequest = match serde_json::from_slice(buf) {
        Ok(r) => r,
        Err(e) => return AutomationResponse::err(format!("malformed request: {e}")),
    };
    let is_recursion = mgr.is_automation_pane(pane.0);
    // The pane's workspace stamps origin (R9) so an agent run's tab lands back
    // in it; falls back to empty (→ first workspace) if not yet replicated.
    let workspace_id = attention.pane_workspace(pane).unwrap_or_default();
    // monitor-handoff U4 (R11): the pickup-pointer resolver for a monitor
    // create — starts from the *validated socket caller's* leaf key.
    let pty_for_pointers = Arc::clone(pty);
    let live_cwd = req.cwd.clone();
    let resolve_pointers = move || -> Option<automations::model::MonitorPointers> {
        let leaf_key = pty_for_pointers.leaf_key(pane)?;
        crate::monitor_pointers_from_target(
            session::handoff::resolve_target_now(&leaf_key, live_cwd.as_deref()),
            live_cwd.as_deref(),
        )
    };
    crate::dispatch_automation_op(
        mgr.as_ref(),
        pane.0,
        &workspace_id,
        is_recursion,
        req,
        &resolve_pointers,
    )
}

/// Peer-op entry (agent-peer-messaging U2/U6): parse the request, assemble
/// the live ports, and route to the pure gate sequence. `origin` is the
/// socket's token-resolved pane (KTD2); the wire never carries a sender.
#[allow(clippy::too_many_arguments)]
fn handle_peer_request(
    feed_state: &Arc<feed::FeedState>,
    attention: &Arc<AttentionManager>,
    events: &stream::EventSink,
    origin: PaneId,
    buf: &[u8],
    pty: &Arc<PtyManager>,
    resolver: &Arc<feed::fallback::FallbackResolver>,
    buckets: &Arc<std::sync::Mutex<peer::rate::Buckets>>,
    delivery_lock: &Arc<std::sync::Mutex<()>>,
) -> crate::cli::peer::PeerResponse {
    use crate::cli::peer::{PeerRequest, PeerResponse};

    let req: PeerRequest = match serde_json::from_slice(buf) {
        Ok(r) => r,
        Err(_) => return PeerResponse::err("badRequest"),
    };

    let roster = || feed_state.roster();
    let leaf_for_pane = |p: u64| pty.leaf_key(PaneId(p));
    let try_take = |pane: u64, at: u64| {
        buckets
            .lock()
            .map(|mut b| b.try_take(pane, at))
            .unwrap_or(false)
    };
    // The wide question gate (KTD5).
    let ask_pending = |leaf: &str| match feed_state.agent_gate(leaf) {
        None => false,
        Some(gate) => {
            let resolved = resolver.resolve_io(leaf, gate.reason.as_deref(), &gate.status);
            feed::server::drop_blocked_by_question(resolved.question, gate.reason.as_deref())
        }
    };
    // Guarded delivery (KTD4): `deliver_with_guards` with a no-op commit; the
    // delivery mutex spans both writes so concurrent sends can't splice.
    let deliver = |expect: u64, leaf: &str, text: &str| {
        let _serialized = delivery_lock.lock().unwrap_or_else(|p| p.into_inner());
        let outcome = feed::drop::deliver_with_guards_verified(
            expect,
            text,
            || pty.pane_by_leaf(leaf).map(|p| p.0),
            |pane| pty.is_agent(PaneId(pane)),
            |pane, bytes| pty.write(PaneId(pane), bytes),
            || std::thread::sleep(feed::io::SUBMIT_DELAY),
            || Ok(()),
            |pane| pty.wake_if_detached(PaneId(pane)),
            |pane| pty.pane_output_seq(PaneId(pane)),
            std::thread::sleep,
        );
        if matches!(outcome, feed::drop::DropOutcome::Delivered) {
            if let Some(pane) = pty.pane_by_leaf(leaf) {
                if let Some(o) = attention.on_input(pane) {
                    events(
                        stream::PANE_ATTENTION_EVENT,
                        stream::attention_event_payload(pane, &o),
                    );
                }
            }
        }
        outcome
    };
    peer::dispatch_peer_op(
        origin.0,
        &req,
        &peer::PeerPorts {
            now_ms: notify::now_unix_ms(),
            roster: &roster,
            leaf_for_pane: &leaf_for_pane,
            try_take_rate: &try_take,
            ask_pending: &ask_pending,
            deliver: &deliver,
        },
    )
}

/// Composite run dispatcher: agent runs fork on the claimed row's headless
/// marker; script runs go to the runner (moved verbatim from `lib.rs`).
pub(crate) struct CompositeDispatcher {
    pub(crate) agent: std::sync::Arc<dyn automations::Dispatcher>,
    pub(crate) script: std::sync::Arc<automations::script::ScriptRunner>,
    pub(crate) headless: std::sync::Arc<automations::headless::HeadlessRunner>,
}

impl automations::Dispatcher for CompositeDispatcher {
    fn dispatch_agent(
        &self,
        a: &automations::model::Automation,
        run_id: &str,
        launch: &automations::ResolvedLaunch,
        headless: bool,
    ) -> Result<(), String> {
        // Headless-agent-automations U3: the fork widens from `a.monitor` to
        // the claimed row's marker (threaded by the manager) — a monitor's
        // marker is always true, and a regular agent claim carries its
        // resolved disposition (R1/R2), so one branch serves both.
        if headless {
            // Headless-monitor-checks R1: backend-owned — hand it to the
            // runner and return Ok ("handed off", not "succeeded"); every
            // failure, spawn included, routes through its CheckCloser.
            self.headless.run(a, run_id, launch);
            return Ok(());
        }
        self.agent.dispatch_agent(a, run_id, launch, headless)
    }
    fn dispatch_script(
        &self,
        a: &automations::model::Automation,
        run_id: &str,
    ) -> Result<(), String> {
        self.script.dispatch_script(a, run_id)
    }
}
