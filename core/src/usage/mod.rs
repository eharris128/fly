//! Live plan-usage snapshot for the agent dashboard — the data behind Claude
//! Code's `/usage` rate-limit gauges.
//!
//! `/usage` shows two different things from two sources: a *breakdown* attributed
//! to skills/subagents/MCP (computed locally from the transcript store) and the
//! *plan-limit gauges* (Session 5h window, Weekly all-models, per-model weekly).
//! The gauges are account-wide state — they reflect usage across all your
//! devices — so they can't be derived locally; Claude Code fetches them from
//! `GET https://api.anthropic.com/api/oauth/usage` (observed in the CLI as
//! `fetchUtilization: GET /api/oauth/usage`). This module reproduces that fetch.
//!
//! Auth is the subscription OAuth bearer token Claude Code stores at
//! `~/.claude/.credentials.json` (`claudeAiOauth.accessToken`). fly only ever
//! **reads** that file — it never writes under `~/.claude` (mirrors
//! [`crate::session::transcript`], which reads the transcript store the same way
//! and honors `CLAUDE_CONFIG_DIR`).
//!
//! Caveat: `/api/oauth/usage` is an **internal, undocumented** endpoint — there
//! is no public contract, so the wire shape is parsed defensively (every field
//! optional / `#[serde(default)]`) and a shape change degrades to an empty or
//! partial panel, never a crash. It can change without notice on a CLI update.
//! The dashboard fetches it on open only (KTD-C), never on a timer, to stay well
//! under any rate limit.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod gate;

/// The endpoint behind `/usage`'s gauges.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The OAuth beta header Claude Code sends with `/api/oauth/*` calls.
const OAUTH_BETA: &str = "oauth-2025-04-20";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Total round-trip budget for the fetch. `/api/oauth/usage` has highly variable
/// latency (observed 2.8s–9.2s back-to-back), so a tight cap clips the slow tail
/// and surfaces a *false* "usage request failed: …timed out" in the panel even
/// though the endpoint is fine. Fetched on open only (KTD-C), so a generous
/// budget just means a longer "Loading…" spinner on a genuinely dead network,
/// which beats a transient error flash.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `<claude config dir>/.credentials.json`. Honors `CLAUDE_CONFIG_DIR` (Claude's
/// own config-dir override) when set, else `$HOME/.claude` — mirroring
/// [`crate::session::transcript`]. `None` when neither resolves.
fn credentials_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join(".credentials.json"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/.credentials.json"))
}

/// The subscription OAuth block in `~/.claude/.credentials.json`.
#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthCreds>,
}

#[derive(Deserialize)]
struct OAuthCreds {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Parse the credentials JSON into the OAuth block (pure — tested without disk).
/// Errors are user-facing one-liners the dashboard renders verbatim.
fn oauth_from_json(body: &str) -> Result<OAuthCreds, String> {
    let creds: Credentials =
        serde_json::from_str(body).map_err(|e| format!("can't parse Claude credentials: {e}"))?;
    let oauth = creds
        .oauth
        .ok_or("not signed in to a Claude subscription (no claudeAiOauth)")?;
    if oauth.access_token.as_deref().unwrap_or("").is_empty() {
        return Err("no OAuth token found — run /login in a Claude Code pane".into());
    }
    Ok(oauth)
}

/// Read + parse the stored OAuth credentials (the one IO step before the fetch).
fn read_oauth() -> Result<OAuthCreds, String> {
    let path = credentials_path()
        .ok_or("no HOME or CLAUDE_CONFIG_DIR set to locate Claude credentials")?;
    let body = std::fs::read_to_string(&path)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?;
    oauth_from_json(&body)
}

// ---- wire shapes (defensive: every field optional / defaulted) -------------

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    limits: Vec<RawLimit>,
}

/// A normalized `limits[]` entry — the shape Claude Code itself renders the
/// gauges from. `percent` is an integer-valued number in practice but typed
/// `f64` so an int or a float both parse.
#[derive(Deserialize)]
struct RawLimit {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    percent: f64,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Option<RawScope>,
    #[serde(default)]
    is_active: bool,
}

#[derive(Deserialize)]
struct RawScope {
    #[serde(default)]
    model: Option<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    #[serde(default)]
    display_name: Option<String>,
}

// ---- frontend payload ------------------------------------------------------

/// One plan-limit gauge — a row in the dashboard usage panel.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimit {
    /// e.g. `session`, `weekly_all`, `weekly_scoped`, `overage`.
    pub kind: Option<String>,
    pub group: Option<String>,
    /// Percent of the window consumed, 0–100.
    pub percent: f64,
    /// e.g. `normal`, `warning`, `critical` — drives the bar color.
    pub severity: Option<String>,
    /// ISO 8601 reset time, or null.
    pub resets_at: Option<String>,
    /// Model display name for a per-model (`weekly_scoped`) limit, else null.
    pub scope_label: Option<String>,
    /// Whether this window is the one currently binding (Claude's `is_active`).
    pub is_active: bool,
}

/// The dashboard usage snapshot — the live gauges behind `/usage`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub limits: Vec<UsageLimit>,
    /// The subscription tier from the stored credentials (e.g. `max`, `pro`).
    pub plan: Option<String>,
}

/// Map a parsed usage response (+ the plan label from credentials) into the
/// frontend snapshot (pure — tested against a captured real response).
fn snapshot_from_json(body: &str, plan: Option<String>) -> Result<UsageSnapshot, String> {
    let raw: RawUsage =
        serde_json::from_str(body).map_err(|e| format!("couldn't parse usage response: {e}"))?;
    let limits = raw
        .limits
        .into_iter()
        .map(|l| UsageLimit {
            kind: l.kind,
            group: l.group,
            percent: l.percent,
            severity: l.severity,
            resets_at: l.resets_at,
            scope_label: l.scope.and_then(|s| s.model).and_then(|m| m.display_name),
            is_active: l.is_active,
        })
        .collect();
    Ok(UsageSnapshot { limits, plan })
}

/// Fetch the live plan-usage snapshot (the data behind `/usage`'s gauges).
///
/// Returns `Err(message)` for any failure — not signed in, network error,
/// non-2xx, unparseable — so the dashboard shows a one-line reason instead of a
/// blank panel. Called on dashboard open only (KTD-C), never on a timer.
pub async fn usage_snapshot() -> Result<UsageSnapshot, String> {
    fetch_snapshot(REQUEST_TIMEOUT).await
}

/// The shared request core behind [`usage_snapshot`] (dashboard, 30s budget)
/// and the automations usage gate ([`gate`], short budget — usage-limit-
/// deferral plan, KTD5). One code path so the two callers can't drift on
/// credentials, headers, or parse.
pub(crate) async fn fetch_snapshot(timeout: Duration) -> Result<UsageSnapshot, String> {
    let oauth = read_oauth()?;
    let plan = oauth.subscription_type.clone();
    let token = oauth.access_token.unwrap_or_default();

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("fly/", env!("CARGO_PKG_VERSION"), " (usage-panel)"))
        .build()
        .map_err(|e| format!("couldn't build HTTP client: {e}"))?;

    let resp = client
        .get(USAGE_URL)
        .bearer_auth(&token)
        .header("anthropic-beta", OAUTH_BETA)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await
        .map_err(|e| format!("usage request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => "Claude sign-in expired — run /login in a pane".into(),
            429 => "rate limited fetching usage — try again shortly".into(),
            s => format!("usage endpoint returned HTTP {s}"),
        });
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("couldn't read usage response: {e}"))?;
    snapshot_from_json(&body, plan)
}

/// The crate's one async runtime (2026-08-27-001 KTD5): lazily built, owned
/// here because the usage fetch is the only async code in a deliberately
/// synchronous crate. Multi-thread with a single worker so `block_on` can be
/// called concurrently from the automations sweep thread (the gate) and a
/// control-connection thread (the dashboard one-shot) without serializing
/// one behind the other's timeout.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("fly-usage-rt")
            .enable_all()
            .build()
            .expect("usage runtime")
    })
}

/// Run a future to completion on the crate runtime from a plain thread.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    runtime().block_on(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed copy of a real `/api/oauth/usage` 200 response.
    const SAMPLE: &str = r#"{
      "five_hour": { "utilization": 6.0, "resets_at": "2026-06-30T12:49:59+00:00" },
      "seven_day": { "utilization": 28.0, "resets_at": "2026-07-03T12:59:59+00:00" },
      "seven_day_opus": null,
      "seven_day_sonnet": { "utilization": 1.0, "resets_at": "2026-07-03T12:59:59+00:00" },
      "limits": [
        { "kind": "session", "group": "session", "percent": 6, "severity": "normal",
          "resets_at": "2026-06-30T12:49:59+00:00", "scope": null, "is_active": false },
        { "kind": "weekly_all", "group": "weekly", "percent": 28, "severity": "normal",
          "resets_at": "2026-07-03T12:59:59+00:00", "scope": null, "is_active": true },
        { "kind": "weekly_scoped", "group": "weekly", "percent": 1, "severity": "normal",
          "resets_at": "2026-07-03T12:59:59+00:00",
          "scope": { "model": { "id": null, "display_name": "Sonnet" }, "surface": null },
          "is_active": false }
      ]
    }"#;

    #[test]
    fn parses_limits_from_real_response() {
        let snap = snapshot_from_json(SAMPLE, Some("max".into())).unwrap();
        assert_eq!(snap.plan.as_deref(), Some("max"));
        assert_eq!(snap.limits.len(), 3);

        let session = &snap.limits[0];
        assert_eq!(session.kind.as_deref(), Some("session"));
        assert_eq!(session.percent, 6.0);
        assert!(!session.is_active);
        assert_eq!(session.scope_label, None);

        let weekly = &snap.limits[1];
        assert_eq!(weekly.kind.as_deref(), Some("weekly_all"));
        assert!(weekly.is_active);

        // A per-model limit lifts the model name out of the nested scope.
        let scoped = &snap.limits[2];
        assert_eq!(scoped.kind.as_deref(), Some("weekly_scoped"));
        assert_eq!(scoped.scope_label.as_deref(), Some("Sonnet"));
    }

    #[test]
    fn missing_limits_array_yields_empty_not_error() {
        // A shape with no `limits` key degrades to an empty panel, never an error.
        let snap = snapshot_from_json(r#"{"five_hour": null}"#, None).unwrap();
        assert!(snap.limits.is_empty());
        assert_eq!(snap.plan, None);
    }

    #[test]
    fn malformed_json_is_a_clean_error() {
        assert!(snapshot_from_json("not json", None).is_err());
    }

    #[test]
    fn oauth_requires_a_nonempty_token() {
        assert!(oauth_from_json(r#"{"claudeAiOauth":{"accessToken":"abc","subscriptionType":"max"}}"#).is_ok());
        assert!(oauth_from_json(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_err());
        assert!(oauth_from_json(r#"{"other":1}"#).is_err());
    }
}
