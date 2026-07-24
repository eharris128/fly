//! Phone screenshot drop — storage, sniffing, and prompt composition
//! (phone-screenshot-drop U1–U3).
//!
//! This module owns everything between "bytes arrive on the wire" and "text is
//! ready for the pane": where the image lands (this file's path helpers),
//! what format it actually is (`sniff_image`, U2), how it is written durably
//! (`DropStore`, U2), and the exact text the agent sees (`compose_drop_prompt`,
//! U3). The route that drives it lives in `server.rs` (U6).
//!
//! Everything here is pure or filesystem-only — no Tauri, no PTY, no HTTP — so
//! it is unit-testable without a running app.

use std::fmt;
use std::path::{Path, PathBuf};

/// Why a configured `feed.dropDir` could not be turned into a usable absolute
/// path. Every variant is a *refusal*, never a silent fallback: resolving a bad
/// shape against the process cwd would put screenshots in `/` for a GUI launched
/// from a desktop file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropDirError {
    /// The configured value was empty or entirely whitespace.
    Empty,
    /// Neither absolute nor tilde-prefixed — e.g. `inbox` or `./shots`.
    Relative,
    /// `~user/...`. Expanding this needs a passwd lookup fly does not do.
    ForeignHome,
    /// Tilde-prefixed but `$HOME` could not be determined.
    NoHome,
}

impl fmt::Display for DropDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Empty => "feed.dropDir is empty",
            Self::Relative => "feed.dropDir must be absolute or start with `~/`",
            Self::ForeignHome => "feed.dropDir uses `~user/`, which fly does not expand",
            Self::NoHome => "feed.dropDir starts with `~` but $HOME is unset",
        };
        f.write_str(msg)
    }
}

/// Expand a leading `~` against `home` and require the result be absolute.
///
/// Pure — `home` is injected rather than read from the environment, so the
/// tests below pin the expansion without touching `$HOME`.
///
/// This is deliberately **not** applied at deserialization. `set_config`
/// round-trips the whole `Config` back to disk, so an expansion baked into the
/// in-memory struct would be persisted the first time the settings menu saves
/// anything, silently rewriting the user's `~/projects/inbox` and freezing
/// `$HOME` into their config file. See [`crate::config::FeedConfig::drop_dir`].
///
/// Absoluteness is enforced here rather than in a second validator so there is
/// exactly one place that decides what a legal `dropDir` looks like.
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> Result<PathBuf, DropDirError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DropDirError::Empty);
    }
    let Some(rest) = raw.strip_prefix('~') else {
        // No tilde: the only other legal shape is an absolute path.
        return if Path::new(raw).is_absolute() {
            Ok(PathBuf::from(raw))
        } else {
            Err(DropDirError::Relative)
        };
    };
    // `~`, `~/…`, or `~user/…` — only the first two are ours to expand.
    let tail = match rest {
        "" => None,
        r if r.starts_with('/') => Some(r.trim_start_matches('/')),
        _ => return Err(DropDirError::ForeignHome),
    };
    let home = home.ok_or(DropDirError::NoHome)?;
    Ok(match tail {
        // `~/` with nothing after it is still just `$HOME`.
        Some(t) if !t.is_empty() => home.join(t),
        _ => home.to_path_buf(),
    })
}

/// Resolve the effective drop directory: the configured value if set, else
/// `<data root>/inbox` (KTD4).
///
/// The unconfigured default goes under the data root — and so *does* stay
/// per-flavor isolated — while an explicitly configured directory is taken at
/// face value. That asymmetry is intended: the default has to put files
/// somewhere defensible, but a user who names a directory means that directory.
pub fn resolve_drop_dir(
    configured: Option<&str>,
    home: Option<&Path>,
    data_root: &Path,
) -> Result<PathBuf, DropDirError> {
    match configured {
        Some(raw) => expand_tilde(raw, home),
        None => Ok(data_root.join("inbox")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    #[test]
    fn tilde_slash_expands_against_home() {
        let got = expand_tilde("~/projects/inbox", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/projects/inbox"));
    }

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(expand_tilde("~", Some(&home())).unwrap(), home());
        // `~/` is the same place — a trailing slash is not a subdirectory.
        assert_eq!(expand_tilde("~/", Some(&home())).unwrap(), home());
    }

    #[test]
    fn absolute_path_passes_through_unchanged() {
        let got = expand_tilde("/srv/shots", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/srv/shots"));
    }

    #[test]
    fn absolute_path_does_not_need_home() {
        assert!(expand_tilde("/srv/shots", None).is_ok());
    }

    /// The case the whole helper exists for: a bare relative path must be
    /// refused, never resolved against the process cwd (`/` for a GUI launched
    /// from a desktop file).
    #[test]
    fn bare_relative_path_is_rejected() {
        assert_eq!(expand_tilde("inbox", Some(&home())), Err(DropDirError::Relative));
        assert_eq!(
            expand_tilde("./inbox", Some(&home())),
            Err(DropDirError::Relative)
        );
        assert_eq!(
            expand_tilde("../inbox", Some(&home())),
            Err(DropDirError::Relative)
        );
    }

    #[test]
    fn empty_and_whitespace_are_rejected() {
        assert_eq!(expand_tilde("", Some(&home())), Err(DropDirError::Empty));
        assert_eq!(expand_tilde("   ", Some(&home())), Err(DropDirError::Empty));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let got = expand_tilde("  ~/inbox  ", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/inbox"));
    }

    #[test]
    fn foreign_home_is_rejected_not_guessed() {
        assert_eq!(
            expand_tilde("~root/inbox", Some(&home())),
            Err(DropDirError::ForeignHome)
        );
    }

    #[test]
    fn tilde_without_home_is_rejected() {
        assert_eq!(expand_tilde("~/inbox", None), Err(DropDirError::NoHome));
        assert_eq!(expand_tilde("~", None), Err(DropDirError::NoHome));
    }

    #[test]
    fn unconfigured_defaults_under_the_data_root() {
        let got = resolve_drop_dir(None, Some(&home()), Path::new("/data/fly")).unwrap();
        assert_eq!(got, PathBuf::from("/data/fly/inbox"));
    }

    #[test]
    fn configured_value_wins_over_the_data_root() {
        let got =
            resolve_drop_dir(Some("~/projects/inbox"), Some(&home()), Path::new("/data/fly"))
                .unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/projects/inbox"));
    }

    #[test]
    fn configured_bad_shape_propagates_rather_than_falling_back() {
        let got = resolve_drop_dir(Some("inbox"), Some(&home()), Path::new("/data/fly"));
        assert_eq!(got, Err(DropDirError::Relative));
    }
}
