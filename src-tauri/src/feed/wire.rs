//! The feed wire contract (U1): the JSON shape an external, local consumer
//! (the `game` portfolio) reads over the read-only SSE endpoint.
//!
//! This is the single source of truth for the boundary shape and is mirrored
//! by `src/lib/feed.ts` on the frontend — the `AgentEntry` half is *pushed*
//! from the webview (it computes the grouped roster, which has no backend
//! equivalent), the `AutomationEntry` half is assembled backend-side from the
//! authoritative automations store. Serde is **camelCase**, matching the
//! automations wire contract (`automations/model.rs`), because the same field
//! names cross the socket-free HTTP boundary into a TypeScript consumer.

use serde::{Deserialize, Serialize};

/// One in-flight agent, as the dashboard already models it (`src/lib/home.ts`
/// `AgentRow`). Keyed by the stable `leafKey` (never the reassignable paneId —
/// same invariant the scrollback + resume stores rest on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub leaf_key: String,
    /// Owning workspace name.
    pub workspace: String,
    /// Owning tab's display title.
    pub tab: String,
    /// This pane's cwd (null when unknown), for a per-row label.
    pub cwd: Option<String>,
    /// `working` | `waiting` | `idle` | `running` — the dashboard's precedence
    /// (raised→waiting, acknowledged→idle, output stretch→working, live
    /// background work upgrades idle→running).
    pub status: String,
    /// The pane is raised *and unseen* (needs the user).
    pub needs_attention: bool,
    /// Why it needs you (`question`/`permission`/`finished`/`alert`), only on a
    /// raised row; null otherwise.
    pub reason: Option<String>,
    /// Current work stretch in ms, or null when idle.
    pub working_for_ms: Option<f64>,
    /// Effective (rise-debounced) live background-task count — the `running · N`
    /// number; 0 for any non-`running` row.
    pub live_task_count: u32,
    /// Stable jump number (1–9, then 0 for the tenth), or null past ten.
    pub num: Option<u32>,
}

/// One automation, projected from `automations::model::Automation` + its derived
/// last run. Read-only: schedule (`cron`/`timezone`) plus the next occurrence and
/// the last run's outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationEntry {
    pub id: String,
    pub name: String,
    /// 5-field cron expression (opaque; the consumer humanizes).
    pub cron: String,
    /// IANA timezone name.
    pub timezone: String,
    /// Whether the automation is active (a paused one is `false`).
    pub enabled: bool,
    /// Next occurrence, epoch ms; null when paused.
    pub next_run_at: Option<u64>,
    /// Last run's status (`running`/`succeeded`/`failed`/`skipped`), or null if
    /// it has never run.
    pub last_status: Option<String>,
    /// When the last run reached a terminal status (or started, for a still-
    /// running row), epoch ms; null if it has never run.
    pub last_run_at: Option<u64>,
}

/// The full snapshot streamed on every SSE frame. `version` is the monotonic
/// counter the server bumps on any change; `emittedAt` is the frame's stamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedSnapshot {
    pub version: u64,
    pub emitted_at: u64,
    pub agents: Vec<AgentEntry>,
    pub automations: Vec<AutomationEntry>,
}

impl AutomationEntry {
    /// Project a stored automation (+ its derived last run) into the wire shape.
    pub fn from_automation(a: &crate::automations::model::Automation) -> Self {
        let last = a.last_run();
        Self {
            id: a.id.clone(),
            name: a.name.clone(),
            cron: a.cron.clone(),
            timezone: a.timezone.clone(),
            enabled: a.enabled,
            next_run_at: a.next_run_at,
            last_status: last.map(|r| run_status_str(&r.status).to_string()),
            // A terminal row carries finished_at; a still-running one only
            // started_at — fall back so a live run still stamps a time.
            last_run_at: last.and_then(|r| r.finished_at.or(r.started_at)),
        }
    }
}

/// Lowercase wire spelling for a run status (matches the camelCase/lowercase
/// convention the frontend consumes).
fn run_status_str(status: &crate::automations::model::RunStatus) -> &'static str {
    use crate::automations::model::RunStatus;
    match status {
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_camel_case() {
        let snap = FeedSnapshot {
            version: 7,
            emitted_at: 1_700_000_000_000,
            agents: vec![AgentEntry {
                leaf_key: "ws-1/tab-1/leaf-1".into(),
                workspace: "home".into(),
                tab: "fly".into(),
                cwd: Some("/home/evan/projects/fly".into()),
                status: "working".into(),
                needs_attention: false,
                reason: None,
                working_for_ms: Some(4200.0),
                live_task_count: 2,
                num: Some(1),
            }],
            automations: vec![AutomationEntry {
                id: "a1".into(),
                name: "check tests".into(),
                cron: "*/5 * * * *".into(),
                timezone: "UTC".into(),
                enabled: true,
                next_run_at: Some(1_700_000_300_000),
                last_status: Some("succeeded".into()),
                last_run_at: Some(1_700_000_000_000),
            }],
        };
        let v = serde_json::to_value(&snap).unwrap();
        // Golden camelCase keys the `game` consumer relies on.
        assert_eq!(v["version"], 7);
        assert_eq!(v["emittedAt"], 1_700_000_000_000u64);
        assert_eq!(v["agents"][0]["leafKey"], "ws-1/tab-1/leaf-1");
        assert_eq!(v["agents"][0]["needsAttention"], false);
        assert_eq!(v["agents"][0]["workingForMs"], 4200.0);
        assert_eq!(v["agents"][0]["liveTaskCount"], 2);
        assert_eq!(v["automations"][0]["cron"], "*/5 * * * *");
        assert_eq!(v["automations"][0]["nextRunAt"], 1_700_000_300_000u64);
        assert_eq!(v["automations"][0]["lastStatus"], "succeeded");
        // And it round-trips back byte-for-byte.
        let back: FeedSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn empty_roster_serializes_to_empty_arrays() {
        let snap = FeedSnapshot {
            version: 0,
            emitted_at: 0,
            agents: vec![],
            automations: vec![],
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert!(v["agents"].as_array().unwrap().is_empty());
        assert!(v["automations"].as_array().unwrap().is_empty());
    }
}
