//! Best-effort secret scrub for captured agent output (automations-workspace-
//! and-model plan, U4b — the redaction decision resolved in the plan's Open
//! Questions: *scrub before persist*).
//!
//! Agent-mode automation runs launch with `--dangerously-skip-permissions`, so
//! an agent can read `.env` / credential files and quote a secret in its final
//! summary. That summary is captured into the run row, which is shown in the
//! dashboard and printed by `fly automation runs --output`. This pass masks the
//! common secret shapes before the text is persisted (the store is also 0600).
//!
//! This is **defense-in-depth, not a guarantee**: a novel secret shape can slip
//! through. It deliberately errs toward over-redaction (masking an env-style
//! assignment's value is cheaper than leaking one). No regex dependency — a
//! small hand-rolled scan keeps the build offline-friendly.
//!
//! **Call-order contract** (the `feed/io.rs::clean` convention, applied to the
//! automations capture path by monitor-handoff U3, per the accepted residual
//! finding in `docs/residual-review-findings/feat-feed-pending-question.md`):
//! **sanitize → scrub → truncate**. Control-sanitize the text *before* calling
//! [`scrub_secrets`] — this scan matches tokens by prefix/shape, which a
//! zero-width or control character *inside* a token defeats (`sk-\u{200B}ant-…`
//! slips past, and a later sanitize pass would re-form the cleartext). Truncate
//! last, so the scrub always sees full, unstraddled values. The order lives in
//! ONE place: [`clean_text`] / [`clean_captured`] (headless-monitor-checks U5,
//! R8) — every capture path calls those, never the composition inline, so the
//! invariant cannot drift between paths.

/// The placeholder every match collapses to.
const MASK: &str = "[redacted]";

/// The module-doc call-order contract as one named step (headless-monitor-
/// checks U5, R8): **sanitize control chars FIRST, then scrub** — a control /
/// zero-width char inside a token would otherwise split it past the prefix/
/// shape scan and a later sanitize pass would re-form the cleartext (the
/// order test below proves the hazard). The tail cap stays OUT of this
/// helper deliberately: it runs at close inside `Automation::close`, after
/// the verdict parse, so R8's parse-before-cap holds by construction.
///
/// This is the raw composition — it preserves an all-whitespace result. Most
/// callers want [`clean_captured`]; the Infra-reason path
/// (`AutomationManager::close_headless_run`) uses this directly because a
/// stored error string must never collapse to `None`.
pub fn clean_text(text: &str) -> String {
    scrub_secrets(&crate::notify::sanitize_multiline(text))
}

/// [`clean_text`] plus the empty→`None` mapping: the shared cleaning helper
/// for captured run output (headless-monitor-checks U5, R8). Both capture
/// paths call it — `lib.rs`'s `OutputCapturer` closure (the pane transcript
/// capture) and `AutomationManager::close_headless_run` (the headless stream
/// result) — so an output that cleans to nothing reads as an *absent*
/// capture on either path (`None`, which the derived broken-monitor counter
/// counts as unreadable).
pub fn clean_captured(text: &str) -> Option<String> {
    let cleaned = clean_text(text);
    (!cleaned.trim().is_empty()).then_some(cleaned)
}

/// Uppercased substrings that mark an env-style assignment key as sensitive:
/// `AWS_SECRET_ACCESS_KEY=…`, `GITHUB_TOKEN=…`, `DB_PASSWORD=…`, … The value
/// after the first `=`/`:` is masked when the key contains any of these.
const SENSITIVE_KEY_MARKERS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "PRIVATE",
    "APIKEY",
    "AUTH",
    "SESSION",
];

/// Known secret-token prefixes. A whitespace-delimited word starting with one
/// of these (and long enough to be a real token) is masked whole.
const SECRET_TOKEN_PREFIXES: &[&str] = &[
    "sk-",          // OpenAI / Anthropic-style API keys (sk-, sk-ant-…)
    "ghp_",         // GitHub personal access token
    "gho_",         // GitHub OAuth token
    "ghs_",         // GitHub server-to-server
    "ghr_",         // GitHub refresh
    "github_pat_",  // GitHub fine-grained PAT
    "xoxb-",        // Slack bot token
    "xoxp-",        // Slack user token
    "xoxa-",        // Slack app token
    "xoxr-",        // Slack refresh token
    "AKIA",         // AWS access key id
    "ASIA",         // AWS temporary access key id
    "AIza",         // Google API key
    "eyJ",          // JWT / base64 `{"` — Bearer tokens are commonly JWTs
];

/// A word after which the *next* word is a bare secret value to mask: a
/// `KEY:` / `KEY=` prefix with a sensitive key whose value is space-separated
/// (`GITHUB_TOKEN: abc`), or a bare `Bearer` / `Authorization` keyword
/// (`Authorization: Bearer <jwt>`). Lets a value survive the tokenizer's
/// whitespace split.
fn is_secret_lead(word: &str) -> bool {
    if let Some(key) = word.strip_suffix(':').or_else(|| word.strip_suffix('=')) {
        if is_sensitive_key(key) {
            return true;
        }
    }
    matches!(word.to_ascii_lowercase().as_str(), "bearer" | "authorization")
}

/// Mask common secret shapes in `input`, returning the scrubbed text. Pure.
pub fn scrub_secrets(input: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_pem = false;
    for line in input.lines() {
        if in_pem {
            // Inside a PEM block: drop every line until the END marker (the
            // placeholder was already emitted at BEGIN).
            if line.contains("-----END") {
                in_pem = false;
            }
            continue;
        }
        if line.contains("-----BEGIN") && line.to_ascii_uppercase().contains("PRIVATE KEY") {
            in_pem = true;
            out.push("[redacted PEM private key]".to_string());
            continue;
        }
        out.push(scrub_line(line));
    }
    out.join("\n")
}

/// Scrub one line, preserving its whitespace runs so formatting survives. A
/// "lead" word (see [`is_secret_lead`]) arms the *next* word for masking; a
/// chain of leads (`Authorization:` → `Bearer` → `<jwt>`) stays armed until the
/// first non-lead word, which is the actual secret.
fn scrub_line(line: &str) -> String {
    let mut out = String::new();
    let mut armed = false;
    for seg in segments(line) {
        // Whitespace runs pass through untouched.
        if seg.chars().next().is_some_and(char::is_whitespace) {
            out.push_str(seg);
            continue;
        }
        if armed {
            if is_secret_lead(seg) {
                // Another lead (e.g. `Bearer` after `Authorization:`): keep it
                // and stay armed for the value that follows.
                out.push_str(seg);
            } else {
                out.push_str(MASK);
                armed = false;
            }
            continue;
        }
        out.push_str(&scrub_word(seg));
        armed = is_secret_lead(seg);
    }
    out
}

/// Scrub a single non-whitespace word: an env-style `KEY=VALUE` / `KEY:VALUE`
/// with a sensitive key, or a known secret-prefixed token.
fn scrub_word(word: &str) -> String {
    // Env-style assignment: mask the value after the first `=` or `:` when the
    // key looks sensitive.
    if let Some(idx) = word.find(['=', ':']) {
        let (key, rest) = word.split_at(idx);
        let sep = &rest[..1];
        let value = &rest[1..];
        if !value.is_empty() && is_sensitive_key(key) {
            return format!("{key}{sep}{MASK}");
        }
    }
    if looks_like_secret_token(word) {
        return MASK.to_string();
    }
    word.to_string()
}

/// Does an assignment key contain a sensitive marker? Case-insensitive
/// substring match — deliberately broad (over-redacting a value is safe).
fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SENSITIVE_KEY_MARKERS.iter().any(|m| upper.contains(m))
}

/// Does a word look like a secret token (known prefix + enough length)? The
/// length floor avoids masking a bare prefix like `sk-` or a short word.
fn looks_like_secret_token(word: &str) -> bool {
    // Trim surrounding quotes/punctuation the token may be wrapped in.
    let w = word.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')'));
    SECRET_TOKEN_PREFIXES
        .iter()
        .any(|p| w.len() >= p.len() + 12 && w.starts_with(p))
}

/// Split `line` into alternating whitespace / non-whitespace runs, preserving
/// every byte so a scrubbed line keeps its original spacing.
fn segments(line: &str) -> Vec<&str> {
    let mut segs = Vec::new();
    let mut start = 0;
    let mut prev_ws: Option<bool> = None;
    for (i, c) in line.char_indices() {
        let ws = c.is_whitespace();
        if let Some(p) = prev_ws {
            if p != ws {
                segs.push(&line[start..i]);
                start = i;
            }
        }
        prev_ws = Some(ws);
    }
    if start < line.len() {
        segs.push(&line[start..]);
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prose_is_untouched() {
        let text = "Disk usage is 82% and climbing.\nTop consumer: ~/projects.";
        assert_eq!(scrub_secrets(text), text);
    }

    #[test]
    fn env_style_assignments_mask_only_the_value() {
        assert_eq!(
            scrub_secrets("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG"),
            "AWS_SECRET_ACCESS_KEY=[redacted]"
        );
        assert_eq!(scrub_secrets("GITHUB_TOKEN: abcdef123456"), "GITHUB_TOKEN: [redacted]");
        assert_eq!(scrub_secrets("DB_PASSWORD=hunter2"), "DB_PASSWORD=[redacted]");
        // Non-sensitive key keeps its value.
        assert_eq!(scrub_secrets("REGION=us-east-1"), "REGION=us-east-1");
    }

    #[test]
    fn known_prefix_tokens_are_masked_whole() {
        assert_eq!(
            scrub_secrets("the key is sk-ant-api03-abcdefghijklmnop rotated"),
            "the key is [redacted] rotated"
        );
        assert_eq!(
            scrub_secrets("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"),
            "[redacted]"
        );
        // A bare prefix (too short to be a real token) is left alone.
        assert_eq!(scrub_secrets("use sk- as the prefix"), "use sk- as the prefix");
    }

    #[test]
    fn bearer_and_authorization_mask_the_following_token() {
        assert_eq!(
            scrub_secrets("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6"),
            "Authorization: Bearer [redacted]"
        );
        assert_eq!(scrub_secrets("Bearer sometokenvalue123"), "Bearer [redacted]");
    }

    #[test]
    fn pem_private_key_block_is_collapsed() {
        let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...\nline2\n-----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(
            scrub_secrets(text),
            "before\n[redacted PEM private key]\nafter"
        );
    }

    #[test]
    fn formatting_and_indentation_survive_a_scrub() {
        // Leading indentation and inner spacing are preserved on scrubbed lines.
        let text = "  summary:\n    TOKEN=abcdefghijkl ok";
        assert_eq!(scrub_secrets(text), "  summary:\n    TOKEN=[redacted] ok");
    }

    // The module-doc call-order contract (monitor-handoff U3): a zero-width
    // char inside a token hides it from this prefix/shape scan, so callers
    // must control-sanitize BEFORE scrubbing (sanitize → scrub → truncate).
    #[test]
    fn sanitize_before_scrub_catches_a_zero_width_split_token() {
        let raw = "the token s\u{200B}k-ant-api03-abcdefghijklmnop leaked";
        // Scrub alone misses the split token — exactly why order matters.
        assert!(
            scrub_secrets(raw).contains("abcdefghijklmnop"),
            "documented hazard: the split token slips past a bare scrub"
        );
        // Sanitize-then-scrub re-joins the token and masks it whole.
        let sane = crate::notify::sanitize_multiline(raw);
        assert_eq!(scrub_secrets(&sane), "the token [redacted] leaked");
    }

    // ---- clean_text / clean_captured (headless-monitor-checks U5, R8) -------

    // The shared helper bakes the order in: a control char (and a zero-width
    // char) INSIDE a secret token would split it past the scrub if the order
    // flipped — through the helper the token is re-joined first and masked
    // whole. This is the same hazard the test above documents, asserted on
    // the one entry both capture paths actually call.
    #[test]
    fn clean_captured_sanitizes_before_scrubbing_so_split_tokens_still_mask() {
        // Zero-width space inside the token (a format char, stripped).
        assert_eq!(
            clean_captured("the token s\u{200B}k-ant-api03-abcdefghijklmnop leaked").as_deref(),
            Some("the token [redacted] leaked")
        );
        // A control char inside the token (BEL, stripped) — same guarantee.
        assert_eq!(
            clean_captured("key: gh\u{7}p_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345").as_deref(),
            Some("key: [redacted]")
        );
    }

    // Empty-after-cleaning maps to None (an absent capture, which the derived
    // broken-monitor counter reads as unreadable) — but only through
    // `clean_captured`; the raw `clean_text` sibling preserves the string for
    // the Infra-reason path, which must never lose its stored error.
    #[test]
    fn clean_captured_maps_empty_after_cleaning_to_none() {
        assert_eq!(clean_captured(""), None);
        assert_eq!(clean_captured("   \n\t"), None, "whitespace-only");
        assert_eq!(
            clean_captured("\u{1b}\u{7}\u{200B}"),
            None,
            "control/format chars alone clean to nothing"
        );
        // The sibling keeps whatever survives cleaning, even pure whitespace.
        assert_eq!(clean_text("   \n\t"), "   \n\t");
        assert_eq!(clean_text("\u{1b}[2Jok"), "[2Jok");
    }
}
