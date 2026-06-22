//! U13 configuration substrate: defaults, overrides, corrupt-file fallback,
//! and separation from the session store.

use std::path::PathBuf;

use fly_lib::config::{load_with_fallback, Config, Renderer};
use fly_lib::state::attention::Reason;

#[test]
fn defaults_load_when_no_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let config = load_with_fallback(&path);
    assert_eq!(config, Config::default());
    assert_eq!(config.leader_key, "ctrl+a");
    assert_eq!(config.renderer, Renderer::Auto);
    assert_eq!(config.font_size, 15);
}

#[test]
fn valid_config_overrides_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        r#"{"leaderKey":"ctrl+b","attentionDebounceMs":250,"fontSize":18,"saveScrollback":true}"#,
    )
    .unwrap();

    let config = load_with_fallback(&path);
    assert_eq!(config.leader_key, "ctrl+b");
    assert_eq!(config.attention_debounce_ms, 250);
    assert_eq!(config.font_size, 18);
    assert!(config.save_scrollback);
    // Unspecified fields keep their defaults.
    assert_eq!(config.renderer, Renderer::Auto);
    assert_eq!(config.scrollback_lines, 10_000);
}

#[test]
fn corrupt_config_backs_up_and_falls_back_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, "{ this is not valid json").unwrap();

    let config = load_with_fallback(&path);
    assert_eq!(config, Config::default());
    let backup = PathBuf::from(format!("{}.corrupt.bak", path.display()));
    assert!(backup.exists(), "corrupt file should be backed up, not overwritten");
}

#[test]
fn old_config_without_notification_keys_loads_new_defaults() {
    // A config file predating the notification-parity work (none of the new
    // keys) must still load — every new field falls back to its default (U23
    // back-compat: the whole-key-absent case).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"leaderKey":"ctrl+b","fontSize":16}"#).unwrap();

    let config = load_with_fallback(&path);
    assert!(!config.notifications_muted_default);
    assert_eq!(
        config.notification_sound.as_deref(),
        Some("message-new-instant")
    );
    assert_eq!(config.notification_command, None);
    for reason in [
        Reason::Question,
        Reason::Permission,
        Reason::Finished,
        Reason::Error,
    ] {
        let e = config.reason_effects.for_reason(reason);
        assert!(
            e.desktop && e.sound && e.record,
            "{reason:?} effects should default all-on"
        );
    }
}

#[test]
fn partial_reason_effects_fill_omitted_reasons_and_effects() {
    // The nested serde(default) case (not just whole-key-absent): only
    // `question` is present, and within it only `desktop`. Omitted *reasons*
    // (permission/finished/error) default all-on, and omitted *effects* within
    // `question` (sound/record) default on — which needs serde(default) on both
    // ReasonEffectsConfig and ReasonEffects.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, r#"{"reasonEffects":{"question":{"desktop":false}}}"#).unwrap();

    let config = load_with_fallback(&path);
    let q = config.reason_effects.question;
    assert!(!q.desktop, "explicitly set off");
    assert!(q.sound, "omitted effect within the reason fills on");
    assert!(q.record, "omitted effect within the reason fills on");

    let p = config.reason_effects.permission;
    assert!(
        p.desktop && p.sound && p.record,
        "an omitted reason fills all-on"
    );
}

#[test]
fn notification_sound_null_parses_to_none_and_command_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    // Explicit null sound → silent; a command with quoted env refs round-trips.
    std::fs::write(
        &path,
        r#"{"notificationSound":null,"notificationCommand":"notify-send \"$FLY_NOTIFICATION_TITLE\""}"#,
    )
    .unwrap();
    let config = load_with_fallback(&path);
    assert_eq!(config.notification_sound, None, "explicit null → silent");
    assert_eq!(
        config.notification_command.as_deref(),
        Some(r#"notify-send "$FLY_NOTIFICATION_TITLE""#)
    );

    // A string sound parses to Some.
    std::fs::write(&path, r#"{"notificationSound":"bell"}"#).unwrap();
    let config = load_with_fallback(&path);
    assert_eq!(config.notification_sound.as_deref(), Some("bell"));
}

#[test]
fn config_lives_under_the_config_dir_not_the_session_store() {
    // The settings file is under the XDG config dir; the disposable session
    // store (U12) lives under the data dir, so a corrupt session never wipes
    // settings.
    let path = fly_lib::config::default_path();
    assert!(path.ends_with("fly/config.json"), "got {path:?}");
}
