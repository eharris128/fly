//! The feed wire contract (U1): the JSON shape an external, local consumer
//! reads over the read-only SSE endpoint.
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
    /// Epoch ms of the agent's pending question, or null when nothing is
    /// pending (feed-pending-question R4). Backend-stamped at emit like
    /// `last_reply_at`: for a **choice** question it is resolver-cache-derived
    /// and equals `/output`'s `question.askedAt`; for a **permission** question
    /// it is best-effort (the attention gate is evaluated independently at emit
    /// and at request — KTD4) and may briefly lead or lag `/output`. A changed
    /// value means a new question. The pushed roster never carries it.
    #[serde(default)]
    pub question_pending_at: Option<u64>,
    /// The pane currently backing this leaf, or null while one is still being
    /// assigned (phone-screenshot-drop U4, R14/KTD6).
    ///
    /// Unlike `leafKey`, this is **identity, not addressing**: pane ids are
    /// monotonic and never reused, so a consumer that echoes it back on a
    /// mutation can be told "that is not the session you picked". Leaf keys are
    /// deliberately stable across respawn — `pane_by_leaf` resolves a key to the
    /// newest *live* pane — so a leaf whose agent exited and was replaced
    /// resolves to the replacement, and only the pane id can tell them apart.
    ///
    /// Null is transient (the webview populates the map on spawn) but reachable,
    /// so a consumer must treat a null-`paneId` row as not-yet-targetable rather
    /// than sending to it and taking a guaranteed refusal.
    ///
    /// Additive and nullable like `lastReplyAt`/`questionPendingAt` before it:
    /// `default` keeps both an older stored payload and an older consumer valid.
    #[serde(default)]
    pub pane_id: Option<u64>,
    /// Whether the human opted this pane into receiving peer messages
    /// (agent-peer-messaging U3, R6/KTD6). **Pushed from the webview** — the
    /// dashboard toggle is deliberately the only surface that can set it: no
    /// socket op, CLI verb, or feed route writes it, so a prompt-injected
    /// agent cannot opt itself (or its victim) into receiving. Session-scoped
    /// by construction (the webview seeds it empty every launch — nothing
    /// persisted for a same-uid process to edit). `default` (= false, closed)
    /// keeps an older pushed payload valid.
    #[serde(default)]
    pub peer_opt_in: bool,
}

/// One selectable option of a pending question, as the wire carries it
/// (feed-pending-question U2; mirrors `session/transcript.rs::QuestionOption`
/// after the U3 scrub/sanitize/truncate pass).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    /// The answer primitive: the 1-based source position the on-screen picker
    /// binds this option to (verified live — a raw digit selects instantly).
    /// A consumer answers an `answerable` question by `POST`ing this string as
    /// `mode:"keys"` input, so it never has to reverse-engineer the keybinding.
    pub key: String,
    pub label: String,
    pub description: String,
}

/// One question of a pending AskUserQuestion batch, on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionSpec {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
    /// The free-text answer primitive (feed-other-answer R2): the digit that
    /// focuses the picker's own "Type something." row — the row Claude Code
    /// appends after the authored options. Present only when a `mode:"other"`
    /// answer can be delivered against this question (answerable shape AND
    /// the digit is known: source-count+1 for a transcript body, the rendered
    /// row's digit for a screen body). `options` never contains this row for
    /// a transcript body but does for a screen body (digit fidelity, R4 of
    /// the screen-fallback plan) — `otherKey` is the one place a consumer
    /// should read it from either way. Omitted, not null, when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_key: Option<String>,
}

/// The pending question riding `GET /agents/{key}/output` while an agent is
/// blocked on an interaction (feed-pending-question R1/R3/R7). Omitted — not
/// null — when nothing is pending (R5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionBody {
    /// Epoch ms the question was asked (the pending `tool_use`'s own stamp).
    /// Equals the frame's `questionPendingAt` for a choice question (R4), and
    /// keys the answer path's `ifAskedAt` guard.
    pub asked_at: u64,
    /// `"choice"` (AskUserQuestion picker) or `"permission"` (tool dialog).
    pub kind: String,
    /// `AskUserQuestion` for a choice; the pending tool's name for a permission.
    pub tool: String,
    /// R7: true only for the one shape v1 answers remotely — a single
    /// single-select question. The consumer must not build answer UX when
    /// false; the input route's guard rejects it anyway.
    pub answerable: bool,
    /// The context sentence above the ask, only when it provably belongs to it
    /// (R2); omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// The question batch (choice kind; empty and omitted for permission).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<QuestionSpec>,
    /// Secret-scrubbed one-line summary of a permission request's input
    /// (permission kind; omitted for choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// Provenance (feed-question-screen-fallback R5): `"screen"` when the
    /// body was synthesized from the pane's rendered terminal grid (Claude
    /// Code ≥ 2.1.206 no longer flushes the ask to the transcript at ask
    /// time); omitted for a transcript-derived body. A screen-derived body's
    /// `askedAt` is the ask-time raise stamp, never a transcript stamp — the
    /// two never mix, so an `ifAskedAt` guard armed against one source can
    /// never pass against the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The last run's healthcheck verdict on the wire (monitor enrichment, U6 of
/// the consumer's own plan, not a fly plan; fly's own `2026-07-11-001-*` is
/// the unrelated feed-other-answer plan. Fly-side record:
/// `docs/notes/2026-07-16-feed-monitor-enrichment.md`):
/// the PASS/FAIL outcome plus its short note, projected from the terminal
/// run's [`crate::automations::model::Verdict`]. A verdict is parsed only from
/// a run whose infra outcome *succeeded* (the check process ran cleanly), so a
/// FAIL verdict rides a `lastStatus: "succeeded"` row — run status alone would
/// show green on a failed experiment, which is exactly why the Wall reads the
/// verdict. Omitted (not null) when the last run carried no parsed verdict —
/// every non-monitor automation, and a monitor's not-done checks — so an older
/// consumer sees today's shape unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictEntry {
    /// `"pass"` | `"fail"` — the machine-readable outcome (the file-wide
    /// lowercase/camelCase wire spelling, not the `PASS`/`FAIL` prompt form).
    pub outcome: String,
    /// The check's short verdict note (already head-capped upstream at close);
    /// empty string when the parse produced no note.
    pub note: String,
}

/// One automation, projected from `automations::model::Automation` + its derived
/// last run. Read-only: schedule (`cron`/`timezone`) plus the next occurrence and
/// the last run's outcome. The monitor-enrichment fields (`monitor`,
/// `retiredAt`, `lastVerdict`) are additive (U6) — an older consumer that
/// ignores them reads the pre-enrichment shape unchanged.
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
    /// Whether this automation is a monitor (a bounded healthcheck that
    /// delivers one verdict and retires), vs. an ordinary recurring
    /// automation (U6). `#[serde(default)]` loads an old payload (no key) as
    /// `false`; always serialized so a plain automation carries `false`.
    #[serde(default)]
    pub monitor: bool,
    /// Whether this automation dispatches **closed-loop headless**
    /// (headless-agent-automations R11): the *effective* disposition —
    /// `monitor || (mode.headless ?? config default)` — resolved at
    /// projection time, so a consumer can tell a closed-loop automation
    /// from a pane-spawning one. Additive, the monitor-enrichment
    /// convention: `#[serde(default)]` loads an old payload as `false`.
    #[serde(default)]
    pub headless: bool,
    /// Next occurrence, epoch ms; null when paused.
    pub next_run_at: Option<u64>,
    /// Last run's status (`running`/`succeeded`/`failed`/`skipped`), or null if
    /// it has never run.
    pub last_status: Option<String>,
    /// When the last run reached a terminal status (or started, for a still-
    /// running row), epoch ms; null if it has never run.
    pub last_run_at: Option<u64>,
    /// When a parsed verdict retired this monitor (epoch ms); null when not
    /// retired (every non-monitor, and a still-live monitor). Serialized
    /// explicitly (null when absent), matching the other `Option` time fields
    /// the consumer reads by key; `#[serde(default)]` loads an old payload as
    /// null (U6).
    #[serde(default)]
    pub retired_at: Option<u64>,
    /// The last run's parsed verdict (U6): the honest pass/fail the Wall
    /// latches on, distinct from `last_status` (a FAIL verdict rides a
    /// `succeeded` run — see [`VerdictEntry`]). Omitted when the last run
    /// carried no verdict; `#[serde(default)]` loads an old payload as None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict: Option<VerdictEntry>,
    /// The dependency edge's upstream automation id (automation-dependencies
    /// R16) — set for a dependent (`--after`) automation so a consumer can
    /// tell "analysis waits on modal" without a second lookup path. Additive,
    /// the monitor-enrichment convention: omitted for every non-dependent,
    /// `#[serde(default)]` loads an old payload as None. `last_status` may
    /// now also read `"withheld"` — a dependent that honestly declined to
    /// run (upstream failed/stale/missing); a new *value* on an existing
    /// field, which consumers must tolerate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// The honest decline reason when the last run is `withheld` (R16):
    /// fly-minted, control-safe by construction (never agent output), e.g.
    /// "upstream failed (exit 1)". Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_withheld_reason: Option<String>,
}

/// What the webview pushes each poll — just the agent half (the automations
/// half is assembled backend-side). Mirrors `FeedPublishPayload` in `feed.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedPublishPayload {
    pub agents: Vec<AgentEntry>,
}

/// One turn of the recent conversation tail riding `GET /agents/{key}/output`
/// (feed-conversation-tail R1/R2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEntry {
    /// `"user"` — a prompt delivered TO the agent, from any source (terminal,
    /// feed input, anywhere) — or `"agent"` — a reply FROM it. Exactly these
    /// two strings (R2).
    pub role: String,
    /// Epoch ms of the turn's transcript entry — the same convention as
    /// `repliedAt`. Always present and numeric: a stampless turn is dropped,
    /// never served unstamped (R2).
    pub at: u64,
    /// The turn's text: control-sanitized, secret-scrubbed, then char-capped
    /// (`io::TURN_CAP`). Truncation carries no wire marker — the same contract
    /// as the question strings (R4).
    pub text: String,
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
    /// The pending question, when the agent is blocked on one
    /// (feed-pending-question R1/R5). Omitted when nothing is pending. During
    /// a pending question `text`/`repliedAt` may legitimately equal the
    /// question's own context sentence (the last text-bearing assistant entry
    /// *is* the context) — expected duplication, not suppressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<QuestionBody>,
    /// The recent conversation tail (feed-conversation-tail R1/R3/R5): oldest
    /// → newest, at most `io::MAX_TURNS` turns, ending with the current reply
    /// (the last turn's `at` equals `repliedAt` — the consumer's correlation
    /// contract). Prompts newer than the reply surface once the next reply
    /// closes them out (KTD2). Omitted — never an empty array — when there is
    /// no stamped reply or no servable history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<TurnEntry>,
}

/// The full snapshot streamed on every SSE frame. `version` is the monotonic
/// counter the server bumps on any change; `emittedAt` is the frame's stamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedSnapshot {
    pub version: u64,
    pub emitted_at: u64,
    /// When the webview last pushed a roster, epoch ms — **not** when this
    /// frame was emitted (phone-screenshot-drop U4, KTD6). Null before the
    /// first push.
    ///
    /// The two stamps answer different questions, and conflating them hides a
    /// real failure. `emittedAt` says the *backend* is alive; frames keep
    /// flowing on the keepalive regardless. But the roster is webview-pushed and
    /// `FeedState` never clears its cache on webview teardown, so a frozen
    /// webview yields fresh-looking frames over a dead agent list — every drop
    /// would then echo a pane id that no longer exists and be refused
    /// `paneChanged` forever, with nothing in the frame to diagnose it.
    ///
    /// This stamp advances on **every** publish call, including one whose roster
    /// is byte-identical to the last (which deliberately does not bump
    /// `version`). That is the point: an idle-but-live webview must remain
    /// distinguishable from a frozen one, and only a stamp that moves without a
    /// content change can do that.
    #[serde(default)]
    pub published_at: Option<u64>,
    pub agents: Vec<AgentEntry>,
    pub automations: Vec<AutomationEntry>,
}

impl AutomationEntry {
    /// Project a stored automation (+ its derived last run) into the wire
    /// shape. `headless_default` is `config.automation_defaults.headless`,
    /// read by the caller at emit time (headless-agent-automations U4) so
    /// the projected `headless` bit matches what a claim would resolve.
    pub fn from_automation(
        a: &crate::automations::model::Automation,
        headless_default: bool,
    ) -> Self {
        let last = a.last_run();
        Self {
            id: a.id.clone(),
            name: a.name.clone(),
            cron: a.cron.clone(),
            timezone: a.timezone.clone(),
            enabled: a.enabled,
            monitor: a.monitor,
            headless: a.monitor || a.mode.resolved_headless(headless_default),
            next_run_at: a.next_run_at,
            last_status: last.map(|r| run_status_str(&r.status).to_string()),
            // A terminal row carries finished_at; a still-running one only
            // started_at — fall back so a live run still stamps a time.
            last_run_at: last.and_then(|r| r.finished_at.or(r.started_at)),
            // U6: a monitor retires in the same store mutation that stamps its
            // verdict, so a retired monitor's `retiredAt`, its `lastRunAt`, and
            // the verdict below all point at that final run.
            retired_at: a.retired_at,
            // U6: the verdict rides the last run because a monitor retires on
            // its first verdict and refuses every later claim — the
            // verdict-bearing run stays `last_run()` permanently.
            last_verdict: last.and_then(|r| r.verdict.as_ref()).map(|v| VerdictEntry {
                outcome: verdict_outcome_str(&v.outcome).to_string(),
                note: v.note.clone(),
            }),
            // Automation-dependencies R16: the edge + the honest decline
            // reason (only when the last run actually withheld).
            after: a.after.as_ref().map(|e| e.upstream_id.clone()),
            last_withheld_reason: last
                .filter(|r| r.status == crate::automations::model::RunStatus::Withheld)
                .and_then(|r| r.error.clone()),
        }
    }
}

/// Lowercase wire spelling for a verdict outcome (the file-wide camelCase/
/// lowercase convention). Distinct from `VerdictOutcome::as_str`, which is the
/// uppercase `PASS`/`FAIL` prompt/display form.
fn verdict_outcome_str(outcome: &crate::automations::model::VerdictOutcome) -> &'static str {
    use crate::automations::model::VerdictOutcome;
    match outcome {
        VerdictOutcome::Pass => "pass",
        VerdictOutcome::Fail => "fail",
        // fly-dag-primitives G1: the third outcome, on the wire as "declined".
        VerdictOutcome::Declined => "declined",
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
        // Automation-dependencies R16: the honest dependency decline — a
        // new value on an existing field; consumers tolerate unknowns.
        RunStatus::Withheld => "withheld",
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
            published_at: Some(1_699_999_999_900),
            agents: vec![AgentEntry {
                leaf_key: "ws-1/tab-1/leaf-1".into(),
                workspace: "home".into(),
                tab: "fly".into(),
                cwd: Some("/home/alice/projects/fly".into()),
                status: "working".into(),
                needs_attention: false,
                reason: None,
                working_for_ms: Some(4200.0),
                live_task_count: 2,
                num: Some(1),
                last_reply_at: Some(1_699_999_999_000),
                question_pending_at: Some(1_700_000_000_500),
                pane_id: Some(42),
                peer_opt_in: false,
            }],
            automations: vec![AutomationEntry {
                id: "a1".into(),
                name: "check tests".into(),
                cron: "*/5 * * * *".into(),
                timezone: "UTC".into(),
                enabled: true,
                monitor: false,
                headless: false,
                next_run_at: Some(1_700_000_300_000),
                last_status: Some("succeeded".into()),
                last_run_at: Some(1_700_000_000_000),
                retired_at: None,
                last_verdict: None,
                after: None,
                last_withheld_reason: None,
            }],
        };
        let v = serde_json::to_value(&snap).unwrap();
        // Golden camelCase keys the external consumer relies on.
        assert_eq!(v["version"], 7);
        assert_eq!(v["emittedAt"], 1_700_000_000_000u64);
        assert_eq!(v["agents"][0]["leafKey"], "ws-1/tab-1/leaf-1");
        assert_eq!(v["agents"][0]["needsAttention"], false);
        assert_eq!(v["agents"][0]["workingForMs"], 4200.0);
        assert_eq!(v["agents"][0]["liveTaskCount"], 2);
        assert_eq!(v["agents"][0]["lastReplyAt"], 1_699_999_999_000u64);
        assert_eq!(v["agents"][0]["questionPendingAt"], 1_700_000_000_500u64);
        assert_eq!(v["automations"][0]["cron"], "*/5 * * * *");
        assert_eq!(v["automations"][0]["nextRunAt"], 1_700_000_300_000u64);
        assert_eq!(v["automations"][0]["lastStatus"], "succeeded");
        // U6 enrichment: a plain automation carries monitor false and an
        // explicit-null retiredAt, and omits lastVerdict entirely.
        assert_eq!(v["automations"][0]["monitor"], false);
        assert!(v["automations"][0]["retiredAt"].is_null());
        assert!(v["automations"][0].get("lastVerdict").is_none());
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
            verdict: None,
            bundle_path: None,
            headless: false,
            session_id: None,
            upstream_run_id: None,
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
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            after: None,
            verdict_gated: false,
            cwd: "/tmp".into(),
            mode: Mode::Agent {
                prompt: "do it".into(),
                model: None,
                effort: None, headless: None,
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
        let e = AutomationEntry::from_automation(&a, false);
        assert_eq!(e.id, "a1");
        assert_eq!(e.name, "nightly");
        assert_eq!(e.cron, "0 2 * * *");
        assert_eq!(e.timezone, "America/New_York");
        assert!(e.enabled);
        assert_eq!(e.next_run_at, Some(2_000));
        assert_eq!(e.last_status.as_deref(), Some("succeeded"));
        // Terminal row → finished_at is the last-run stamp.
        assert_eq!(e.last_run_at, Some(1_200));
        // U6: an ordinary automation projects monitor false, no retirement,
        // and no verdict (its run carried none).
        assert!(!e.monitor);
        assert_eq!(e.retired_at, None);
        assert_eq!(e.last_verdict, None);

        // Headless-agent-automations R11: the projected bit is the EFFECTIVE
        // disposition — the caller's config default fills an unpinned
        // automation, an explicit pin wins, and a monitor always projects
        // true regardless of both.
        assert!(!e.headless, "unpinned + default false");
        assert!(AutomationEntry::from_automation(&a, true).headless, "default fills");
        let mut pinned = a.clone();
        pinned.mode = Mode::Agent {
            prompt: "do it".into(),
            model: None,
            effort: None,
            headless: Some(false),
        };
        assert!(!AutomationEntry::from_automation(&pinned, true).headless, "pin wins");
        let mut mon = a.clone();
        mon.monitor = true;
        assert!(AutomationEntry::from_automation(&mon, false).headless, "monitor forces");

        // Additive back-compat (the monitor-enrichment convention): an old
        // payload without the key still parses, reading false.
        let mut v = serde_json::to_value(&e).unwrap();
        v.as_object_mut().unwrap().remove("headless");
        let back: AutomationEntry = serde_json::from_value(v).unwrap();
        assert!(!back.headless);
    }

    /// U6: a monitor whose last run parsed a FAIL verdict — the case run
    /// status alone gets wrong. The infra run *succeeded* (the check process
    /// completed cleanly), so `lastStatus` is `"succeeded"`, while the honest
    /// outcome the Wall latches on rides `lastVerdict.outcome == "fail"`. The
    /// monitor retired in the same mutation, so `retiredAt` is set and the
    /// verdict-bearing run is `last_run()`.
    #[test]
    fn from_automation_projects_monitor_verdict_and_retirement() {
        use crate::automations::model::{
            Automation, Mode, Origin, RunMode, RunRow, RunStatus, Trigger, Verdict, VerdictOutcome,
        };
        let run = RunRow {
            id: "r1".into(),
            mode: RunMode::Agent,
            trigger: Trigger::Schedule,
            // A verdict is parsed only from a Succeeded infra outcome.
            status: RunStatus::Succeeded,
            pane_id: None,
            model: None,
            effort: None,
            verdict: Some(Verdict {
                outcome: VerdictOutcome::Fail,
                note: "disk still climbing".into(),
            }),
            bundle_path: Some("/bundles/a1-r1.md".into()),
            headless: true,
            session_id: Some("sess-9".into()),
            upstream_run_id: None,
            output: Some("FAIL: disk still climbing".into()),
            exit_code: Some(0),
            error: None,
            scheduled_for: Some(1_000),
            started_at: Some(1_100),
            finished_at: Some(1_200),
        };
        let a = Automation {
            id: "a1".into(),
            name: "disk monitor".into(),
            cron: "*/30 * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            retry_on_interrupt: true,
            monitor: true,
            not_before_ms: Some(500),
            // Retired in the same mutation that closed the verdict run.
            retired_at: Some(1_200),
            pickup_pointers: None,
            after: None,
            verdict_gated: false,
            cwd: "/tmp".into(),
            mode: Mode::Agent {
                prompt: "check disk".into(),
                model: None,
                effort: None, headless: None,
            },
            origin: Origin {
                pane_id: 1,
                workspace_id: "ws".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 1_200,
            next_run_at: None,
            runs: vec![run],
        };
        let e = AutomationEntry::from_automation(&a, false);
        assert!(e.monitor, "the monitor flag rides the wire");
        assert_eq!(e.retired_at, Some(1_200));
        // Run status is succeeded (infra ran clean) but the verdict is fail —
        // the whole reason the consumer must read the verdict, not the status.
        assert_eq!(e.last_status.as_deref(), Some("succeeded"));
        let verdict = e.last_verdict.as_ref().expect("the FAIL verdict projects");
        assert_eq!(verdict.outcome, "fail");
        assert_eq!(verdict.note, "disk still climbing");

        // And the enriched fields cross the wire as camelCase.
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["monitor"], true);
        assert_eq!(v["retiredAt"], 1_200u64);
        assert_eq!(v["lastStatus"], "succeeded");
        assert_eq!(v["lastVerdict"]["outcome"], "fail");
        assert_eq!(v["lastVerdict"]["note"], "disk still climbing");
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
            monitor: false,
            not_before_ms: None,
            retired_at: None,
            pickup_pointers: None,
            after: None,
            verdict_gated: false,
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
        let e = AutomationEntry::from_automation(&a, false);
        assert!(!e.enabled);
        assert_eq!(e.next_run_at, None);
        assert_eq!(e.last_status, None);
        assert_eq!(e.last_run_at, None);
        // U6: no runs → no verdict; a plain automation is not a monitor and
        // never retired.
        assert!(!e.monitor);
        assert_eq!(e.retired_at, None);
        assert_eq!(e.last_verdict, None);
    }

    /// U6 back-compat: an old feed payload (pre-enrichment: no monitor /
    /// retiredAt / lastVerdict keys) still deserializes — an older producer's
    /// frame loads with the new fields defaulted (monitor false, not retired,
    /// no verdict), so a mixed-version pairing never breaks the boundary.
    #[test]
    fn automation_entry_without_enrichment_fields_deserializes_to_defaults() {
        let old = serde_json::json!({
            "id": "a1", "name": "legacy", "cron": "*/5 * * * *",
            "timezone": "UTC", "enabled": true,
            "nextRunAt": 1_700_000_300_000u64,
            "lastStatus": "succeeded",
            "lastRunAt": 1_700_000_000_000u64
        });
        let e: AutomationEntry = serde_json::from_value(old).unwrap();
        assert!(!e.monitor, "absent monitor defaults false");
        assert_eq!(e.retired_at, None, "absent retiredAt defaults null");
        assert_eq!(e.last_verdict, None, "absent lastVerdict defaults None");
        // The pre-enrichment fields still load.
        assert_eq!(e.next_run_at, Some(1_700_000_300_000));
        assert_eq!(e.last_status.as_deref(), Some("succeeded"));
    }

    /// U6: a retired monitor whose verdict was PASS carries its retirement
    /// stamp and a `"pass"` verdict — the pass spelling on the wire is
    /// lowercase, not the `PASS` prompt form.
    #[test]
    fn from_automation_projects_retired_pass_monitor() {
        use crate::automations::model::{
            Automation, Mode, Origin, RunMode, RunRow, RunStatus, Trigger, Verdict, VerdictOutcome,
        };
        let run = RunRow {
            id: "r1".into(),
            mode: RunMode::Agent,
            trigger: Trigger::Schedule,
            status: RunStatus::Succeeded,
            pane_id: None,
            model: None,
            effort: None,
            verdict: Some(Verdict {
                outcome: VerdictOutcome::Pass,
                note: String::new(),
            }),
            bundle_path: None,
            headless: true,
            session_id: None,
            upstream_run_id: None,
            output: Some("PASS".into()),
            exit_code: Some(0),
            error: None,
            scheduled_for: Some(1_000),
            started_at: Some(1_100),
            finished_at: Some(1_200),
        };
        let a = Automation {
            id: "a1".into(),
            name: "deploy watch".into(),
            cron: "*/30 * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            retry_on_interrupt: true,
            monitor: true,
            not_before_ms: None,
            retired_at: Some(1_200),
            pickup_pointers: None,
            after: None,
            verdict_gated: false,
            cwd: "/tmp".into(),
            mode: Mode::Agent {
                prompt: "check deploy".into(),
                model: None,
                effort: None, headless: None,
            },
            origin: Origin {
                pane_id: 1,
                workspace_id: "ws".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 1_200,
            next_run_at: None,
            runs: vec![run],
        };
        let e = AutomationEntry::from_automation(&a, false);
        assert!(e.monitor);
        assert_eq!(e.retired_at, Some(1_200));
        let verdict = e.last_verdict.as_ref().expect("the PASS verdict projects");
        assert_eq!(verdict.outcome, "pass");
        assert_eq!(verdict.note, "");
    }

    #[test]
    fn agent_entry_without_last_reply_at_deserializes_to_null() {
        // Back-compat both ways: an old webview push (no lastReplyAt /
        // questionPendingAt) loads as None, and a never-replied agent
        // serializes an explicit null (the consumer's "never unread" /
        // "nothing pending" state), not an absent key.
        let v = serde_json::json!({
            "leafKey": "leaf-1", "workspace": "home", "tab": "fly",
            "cwd": null, "status": "idle", "needsAttention": false,
            "reason": null, "workingForMs": null, "liveTaskCount": 0, "num": null
        });
        let e: AgentEntry = serde_json::from_value(v).unwrap();
        assert_eq!(e.last_reply_at, None);
        assert_eq!(e.question_pending_at, None);
        let out = serde_json::to_value(&e).unwrap();
        assert!(out["lastReplyAt"].is_null());
        assert!(out["questionPendingAt"].is_null());
    }

    #[test]
    fn output_body_choice_question_round_trips_golden_keys() {
        // The full choice shape the external consumer pins against
        // (feed-pending-question R1/R4/R7).
        let body = AgentOutputBody {
            text: "Pick a lag feel.".into(),
            replied_at: Some(1_700_000_000_000),
            question: Some(QuestionBody {
                asked_at: 1_700_000_001_000,
                kind: "choice".into(),
                tool: "AskUserQuestion".into(),
                answerable: true,
                context: Some("Pick a lag feel.".into()),
                questions: vec![QuestionSpec {
                    question: "Lag feel?".into(),
                    header: "Lag".into(),
                    multi_select: false,
                    options: vec![QuestionOption {
                        key: "1".into(),
                        label: "Snappy".into(),
                        description: "Fast and tight".into(),
                    }],
                    other_key: Some("2".into()),
                }],
                request: None,
                source: None,
            }),
            turns: vec![],
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["question"]["askedAt"], 1_700_000_001_000u64);
        assert_eq!(v["question"]["kind"], "choice");
        assert_eq!(v["question"]["tool"], "AskUserQuestion");
        assert_eq!(v["question"]["answerable"], true);
        assert_eq!(v["question"]["context"], "Pick a lag feel.");
        assert_eq!(v["question"]["questions"][0]["question"], "Lag feel?");
        assert_eq!(v["question"]["questions"][0]["header"], "Lag");
        assert_eq!(v["question"]["questions"][0]["multiSelect"], false);
        assert_eq!(v["question"]["questions"][0]["options"][0]["key"], "1");
        assert_eq!(v["question"]["questions"][0]["options"][0]["label"], "Snappy");
        assert_eq!(
            v["question"]["questions"][0]["options"][0]["description"],
            "Fast and tight"
        );
        // The free-text row's digit rides as camelCase otherKey
        // (feed-other-answer R2).
        assert_eq!(v["question"]["questions"][0]["otherKey"], "2");
        // Permission-only key absent on a choice.
        assert!(v["question"].get("request").is_none());
        // And it round-trips back byte-equal.
        let back: AgentOutputBody = serde_json::from_value(v).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn a_spec_without_other_key_omits_it_and_old_bodies_deserialize() {
        // Back-compat both ways (feed-other-answer R2): no otherKey → the key
        // is absent (not null), and a pre-otherKey body still loads as None.
        let spec = QuestionSpec {
            question: "Which?".into(),
            header: String::new(),
            multi_select: false,
            options: vec![],
            other_key: None,
        };
        let v = serde_json::to_value(&spec).unwrap();
        assert!(v.get("otherKey").is_none());
        let old = serde_json::json!({
            "question": "Which?", "header": "", "multiSelect": false, "options": []
        });
        let back: QuestionSpec = serde_json::from_value(old).unwrap();
        assert_eq!(back.other_key, None);
    }

    #[test]
    fn output_body_permission_question_round_trips_golden_keys() {
        let body = AgentOutputBody {
            text: String::new(),
            replied_at: None,
            question: Some(QuestionBody {
                asked_at: 1_700_000_002_000,
                kind: "permission".into(),
                tool: "Bash".into(),
                answerable: false,
                context: None,
                questions: vec![],
                request: Some("cargo build".into()),
                source: None,
            }),
            turns: vec![],
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["question"]["kind"], "permission");
        assert_eq!(v["question"]["tool"], "Bash");
        assert_eq!(v["question"]["answerable"], false);
        assert_eq!(v["question"]["request"], "cargo build");
        // Choice-only keys absent on a permission.
        assert!(v["question"].get("questions").is_none());
        assert!(v["question"].get("context").is_none());
        let back: AgentOutputBody = serde_json::from_value(v).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn output_body_without_question_omits_the_key() {
        // R5: no pending question → the `question` key is absent (not null),
        // so a consumer ignoring the new field sees today's body unchanged.
        let body = AgentOutputBody {
            text: "done".into(),
            replied_at: Some(1),
            question: None,
            turns: vec![],
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("question").is_none());
        // And an old-style body (no question key) still deserializes.
        let old = serde_json::json!({"text": "done", "repliedAt": 1});
        let back: AgentOutputBody = serde_json::from_value(old).unwrap();
        assert_eq!(back.question, None);
    }

    #[test]
    fn output_body_turns_round_trip_golden_keys() {
        // feed-conversation-tail R1/R2/R3: the exact keys + role strings the
        // consumer pins against, oldest → newest, ending at the reply.
        let body = AgentOutputBody {
            text: "All tests pass.".into(),
            replied_at: Some(1_700_000_000_000),
            question: None,
            turns: vec![
                TurnEntry {
                    role: "user".into(),
                    at: 1_699_999_990_000,
                    text: "run the tests".into(),
                },
                TurnEntry {
                    role: "agent".into(),
                    at: 1_700_000_000_000,
                    text: "All tests pass.".into(),
                },
            ],
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["turns"][0]["role"], "user");
        assert_eq!(v["turns"][0]["at"], 1_699_999_990_000u64);
        assert_eq!(v["turns"][0]["text"], "run the tests");
        assert_eq!(v["turns"][1]["role"], "agent");
        // R3: the final turn's `at` equals repliedAt.
        assert_eq!(v["turns"][1]["at"], v["repliedAt"]);
        let back: AgentOutputBody = serde_json::from_value(v).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn output_body_without_turns_omits_the_key() {
        // R5: no history → the `turns` key is absent (not an empty array),
        // and everything else stays byte-identical to today's body.
        let body = AgentOutputBody {
            text: "done".into(),
            replied_at: Some(1),
            question: None,
            turns: vec![],
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("turns").is_none());
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"text":"done","repliedAt":1}"#
        );
        // And an old-style body (no turns key) still deserializes.
        let old = serde_json::json!({"text": "done", "repliedAt": 1});
        let back: AgentOutputBody = serde_json::from_value(old).unwrap();
        assert!(back.turns.is_empty());
    }

    /// Back-compat for the additive `paneId` (phone-screenshot-drop U4): a
    /// payload minted by an older webview — or one replayed from an older
    /// stored frame — still deserializes, with the pane id simply absent.
    #[test]
    fn agent_entry_without_pane_id_deserializes_as_none() {
        let old = serde_json::json!({
            "leafKey": "ws-1/tab-1/leaf-1",
            "workspace": "home",
            "tab": "fly",
            "cwd": null,
            "status": "working",
            "needsAttention": false,
            "reason": null,
            "workingForMs": null,
            "liveTaskCount": 0,
            "num": 1
        });
        let back: AgentEntry = serde_json::from_value(old).unwrap();
        assert_eq!(back.pane_id, None);
        // The two fields added by earlier plans stay absent too — this is the
        // same additive-nullable convention, and it must keep holding.
        assert_eq!(back.last_reply_at, None);
        assert_eq!(back.question_pending_at, None);
    }

    /// A snapshot predating `publishedAt` deserializes with it absent, so a
    /// consumer reading an older frame sees "unknown", not "stale".
    #[test]
    fn snapshot_without_published_at_deserializes_as_none() {
        let old = serde_json::json!({
            "version": 1,
            "emittedAt": 5,
            "agents": [],
            "automations": []
        });
        let back: FeedSnapshot = serde_json::from_value(old).unwrap();
        assert_eq!(back.published_at, None);
    }

    #[test]
    fn pane_id_rides_the_wire_as_camel_case() {
        let json = serde_json::to_value(AgentEntry {
            leaf_key: "l".into(),
            workspace: "w".into(),
            tab: "t".into(),
            cwd: None,
            status: "working".into(),
            needs_attention: false,
            reason: None,
            working_for_ms: None,
            live_task_count: 0,
            num: None,
            last_reply_at: None,
            question_pending_at: None,
            pane_id: Some(17),
            peer_opt_in: true,
        })
        .unwrap();
        assert_eq!(json["paneId"], 17);
        assert_eq!(json["peerOptIn"], true);
    }

    // agent-peer-messaging U3: the opt-in bit is additive — an entry pushed by
    // an older webview (no `peerOptIn` key) deserializes closed, never open.
    #[test]
    fn agent_entry_without_peer_opt_in_deserializes_closed() {
        let old = serde_json::json!({
            "leafKey": "l", "workspace": "w", "tab": "t", "cwd": null,
            "status": "idle", "needsAttention": false, "reason": null,
            "workingForMs": null, "liveTaskCount": 0, "num": null
        });
        let back: AgentEntry = serde_json::from_value(old).unwrap();
        assert!(!back.peer_opt_in, "absent opt-in is closed (KTD6)");
    }

    #[test]
    fn empty_roster_serializes_to_empty_arrays() {
        let snap = FeedSnapshot {
            version: 0,
            emitted_at: 0,
            published_at: None,
            agents: vec![],
            automations: vec![],
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert!(v["agents"].as_array().unwrap().is_empty());
        assert!(v["automations"].as_array().unwrap().is_empty());
    }

    // Automation-dependencies R16: the projection is additive — a
    // non-dependent automation serializes byte-identically to before (no
    // `after`, no `lastWithheldReason`), while a dependent whose last run
    // withheld carries the edge, the `"withheld"` status value, and the
    // fly-minted reason.
    #[test]
    fn dependency_projection_is_additive_and_carries_the_withheld_reason() {
        use crate::automations::model::{
            Automation, Dependency, Mode, Origin, Trigger,
        };
        let mut a = Automation {
            id: "d1".into(),
            name: "feed analysis".into(),
            cron: "22 9 * * 1-5".into(),
            timezone: "Europe/Berlin".into(),
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
                timeout_ms: 1_000,
            },
            origin: Origin {
                pane_id: 1,
                workspace_id: "ws".into(),
                label: "cli".into(),
            },
            created_at: 0,
            updated_at: 0,
            next_run_at: Some(1_000),
            runs: Vec::new(),
        };

        // Plain automation: neither key appears.
        let v = serde_json::to_value(AutomationEntry::from_automation(&a, true)).unwrap();
        assert!(v.get("after").is_none(), "non-dependent omits the edge");
        assert!(v.get("lastWithheldReason").is_none());

        // Dependent with a withheld last run: edge + status + reason.
        a.after = Some(Dependency {
            upstream_id: "up1".into(),
            within_ms: None,
        });
        a.withhold(2_000, Trigger::Schedule, "upstream failed (exit 1)", "w1");
        let v = serde_json::to_value(AutomationEntry::from_automation(&a, true)).unwrap();
        assert_eq!(v["after"], "up1");
        assert_eq!(v["lastStatus"], "withheld");
        assert_eq!(v["lastWithheldReason"], "upstream failed (exit 1)");
    }
}
