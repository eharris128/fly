//! U13 configuration substrate: defaults, overrides, corrupt-file fallback,
//! and separation from the session store.

use std::path::PathBuf;

use fly_lib::config::{load_with_fallback, Config, Renderer};

#[test]
fn defaults_load_when_no_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let config = load_with_fallback(&path);
    assert_eq!(config, Config::default());
    assert_eq!(config.leader_key, "ctrl+a");
    assert_eq!(config.renderer, Renderer::Auto);
}

#[test]
fn valid_config_overrides_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        r#"{"leaderKey":"ctrl+b","attentionDebounceMs":250,"saveScrollback":true}"#,
    )
    .unwrap();

    let config = load_with_fallback(&path);
    assert_eq!(config.leader_key, "ctrl+b");
    assert_eq!(config.attention_debounce_ms, 250);
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
fn config_lives_under_the_config_dir_not_the_session_store() {
    // The settings file is under the XDG config dir; the disposable session
    // store (U12) lives under the data dir, so a corrupt session never wipes
    // settings.
    let path = fly_lib::config::default_path();
    assert!(path.ends_with("fly/config.json"), "got {path:?}");
}
