//! Session identity: marked names, injective leaf-key slugs (KTD4).
//!
//! A session name is `fly-<flavor>-<slug(leafKey)>`. The slug must be
//! *injective* (two distinct leaf keys can never collide onto one session)
//! and confined to `^[a-zA-Z0-9_-]+$` — dots and colons are tmux target
//! syntax and misroute silently (Gas City mining, D4).
//!
//! Escape scheme (injective by construction):
//!   - `a-z A-Z 0-9 -` pass through;
//!   - `_` (the escape introducer) doubles to `__`;
//!   - any other byte becomes `_xx` (lowercase hex of the byte).
//! Decoding is unambiguous: `_` is always followed by either `_` or two hex
//! digits, so the original key round-trips; we never need to decode in
//! production (the store maps name→leaf), but injectivity is what makes the
//! store's reverse map trustworthy.

/// Characters legal in a tmux session name under fly's rules.
fn is_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Injectively encode a leaf key into the tmux-safe charset.
pub fn session_leaf_slug(leaf_key: &str) -> String {
    let mut out = String::with_capacity(leaf_key.len());
    for b in leaf_key.bytes() {
        let c = b as char;
        if is_safe(c) {
            out.push(c);
        } else if c == '_' {
            out.push_str("__");
        } else {
            out.push('_');
            out.push_str(&format!("{b:02x}"));
        }
    }
    out
}

/// The marked session name for a leaf under a flavor (KTD4).
pub fn leaf_session_name(flavor: &str, leaf_key: &str) -> String {
    format!("fly-{}-{}", flavor, session_leaf_slug(leaf_key))
}

/// Validate a name against the tmux-safe charset. Everything fly passes as a
/// `-t`/`-s` target must survive this — including names recovered from the
/// store — so a corrupted store entry cannot smuggle target metacharacters
/// or `run-shell` payload into a tmux invocation (KTD11).
pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty session name".into());
    }
    if let Some(bad) = name
        .chars()
        .find(|&c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return Err(format!(
            "invalid session name {name:?}: character {bad:?} outside [a-zA-Z0-9_-]"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_passes_safe_chars_through() {
        assert_eq!(session_leaf_slug("leaf-12"), "leaf-12");
    }

    #[test]
    fn slug_escapes_underscore_and_unsafe_bytes() {
        assert_eq!(session_leaf_slug("a_b"), "a__b");
        assert_eq!(session_leaf_slug("a.b:c"), "a_2eb_3ac");
        assert_eq!(session_leaf_slug("ws 1"), "ws_201");
    }

    #[test]
    fn slug_is_injective_on_adversarial_pairs() {
        // The classic collision shapes: escape output vs literal input.
        let pairs = [
            ("a_b", "a__b"),
            ("a.b", "a_2eb"),
            ("x_2e", "x."),
            ("_", "__"),
            ("_x", "__x"),
        ];
        for (l, r) in pairs {
            assert_ne!(
                session_leaf_slug(l),
                session_leaf_slug(r),
                "collision between {l:?} and {r:?}"
            );
        }
    }

    #[test]
    fn full_name_is_marked_and_valid() {
        let n = leaf_session_name("fly", "tab-1:leaf.0");
        assert!(n.starts_with("fly-fly-"));
        validate_session_name(&n).unwrap();
    }

    #[test]
    fn validate_rejects_metacharacters_and_empty() {
        assert!(validate_session_name("").is_err());
        for bad in ["a.b", "a:b", "a b", "a'b", "a;b", "a$(x)"] {
            assert!(validate_session_name(bad).is_err(), "{bad:?} accepted");
        }
        validate_session_name("fly-dev-leaf__0").unwrap();
    }
}
