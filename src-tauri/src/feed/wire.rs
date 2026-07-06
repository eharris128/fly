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
    /// Epoch ms of this agent's most recent textual reply, or null if it has
    /// never replied (feed-agent-reply-io U1/R3). **Backend-stamped at frame
    /// emit** from the same resolver `GET /agents/{key}/output` reads, so it
    /// always equals that endpoint's `repliedAt` for the same reply — the
    /// consumer's unread-dot arming depends on the two matching. The webview's
    /// pushed roster never carries it (`default`), keeping old pushes valid.
    #[serde(default)]
    pub last_reply_at: Option<u64>,
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

/// What the webview pushes each poll — just the agent half (the automations
/// half is assembled backend-side). Mirrors `FeedPublishPayload` in `feed.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedPublishPayload {
    pub agents: Vec<AgentEntry>,
}

/// `GET /agents/{key}/output` response body (feed-agent-reply-io U4). An empty
/// `text` with no `repliedAt` is the "no reply yet" state; the consumer reads
/// only these two fields and ignores extras.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOutputBody {
    pub text: String,
    /// Epoch ms of the reply's turn. Omitted (not null) when there is no reply
    /// or the turn carried no parseable stamp — the consumer requires a finite
    /// number, so absence is cleaner than null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replied_at: Option<u64>,
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
                last_reply_at: Some(1_699_999_999_000),
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
        assert_eq!(v["agents"][0]["lastReplyAt"], 1_699_999_999_000u64);
        assert_eq!(v["automations"][0]["cron"], "*/5 * * * *");
        assert_eq!(v["automations"][0]["nextRunAt"], 1_700_000_300_000u64);
        assert_eq!(v["automations"][0]["lastStatus"], "succeeded");
        // And it round-trips back byte-for-byte.
        let back: FeedSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn from_automation_projects_schedule_and_last_run() {
        use crate::automations::model::{
            Automation, Mode, Origin, RunMode, RunRow, RunStatus, Trigger,
        };
        let run = RunRow {
            id: "r1".into(),
            mode: RunMode::Agent,
            trigger: Trigger::Schedule,
            status: RunStatus::Succeeded,
            pane_id: Some(3),
            model: None,
            effort: None,
            output: None,
            exit_code: None,
            error: None,
            scheduled_for: Some(1_000),
            started_at: Some(1_100),
            finished_at: Some(1_200),
        };
        let a = Automation {
            id: "a1".into(),
            name: "nightly".into(),
            cron: "0 2 * * *".into(),
            timezone: "America/New_York".into(),
            enabled: true,
            retry_on_interrupt: false,
            cwd: "/tmp".into(),
            mode: Mode::Agent {
                prompt: "do it".into(),
                model: None,
                effort: None,
            },
            origin: Origin {
                pane_id: 1,
                workspace_id: "ws".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 0,
            next_run_at: Some(2_000),
            runs: vec![run],
        };
        let e = AutomationEntry::from_automation(&a);
        assert_eq!(e.id, "a1");
        assert_eq!(e.name, "nightly");
        assert_eq!(e.cron, "0 2 * * *");
        assert_eq!(e.timezone, "America/New_York");
        assert!(e.enabled);
        assert_eq!(e.next_run_at, Some(2_000));
        assert_eq!(e.last_status.as_deref(), Some("succeeded"));
        // Terminal row → finished_at is the last-run stamp.
        assert_eq!(e.last_run_at, Some(1_200));
    }

    #[test]
    fn from_automation_with_no_runs_has_null_last_run() {
        use crate::automations::model::{Automation, Mode, Origin};
        let a = Automation {
            id: "a2".into(),
            name: "never ran".into(),
            cron: "* * * * *".into(),
            timezone: "UTC".into(),
            enabled: false,
            retry_on_interrupt: false,
            cwd: "/tmp".into(),
            mode: Mode::Script {
                script_file: "s.sh".into(),
                interpreter: "bash".into(),
                timeout_ms: 1000,
            },
            origin: Origin {
                pane_id: 0,
                workspace_id: String::new(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 0,
            next_run_at: None,
            runs: vec![],
        };
        let e = AutomationEntry::from_automation(&a);
        assert!(!e.enabled);
        assert_eq!(e.next_run_at, None);
        assert_eq!(e.last_status, None);
        assert_eq!(e.last_run_at, None);
    }

    #[test]
    fn agent_entry_without_last_reply_at_deserializes_to_null() {
        // Back-compat both ways: an old webview push (no lastReplyAt) loads as
        // None, and a never-replied agent serializes an explicit null (the
        // consumer's "never unread" state), not an absent key.
        let v = serde_json::json!({
            "leafKey": "leaf-1", "workspace": "home", "tab": "fly",
            "cwd": null, "status": "idle", "needsAttention": false,
            "reason": null, "workingForMs": null, "liveTaskCount": 0, "num": null
        });
        let e: AgentEntry = serde_json::from_value(v).unwrap();
        assert_eq!(e.last_reply_at, None);
        let out = serde_json::to_value(&e).unwrap();
        assert!(out["lastReplyAt"].is_null());
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
