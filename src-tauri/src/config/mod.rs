//! Configuration substrate (U13): a typed settings store under the XDG config
//! dir, a *separate* file from the disposable session state (U12), so a corrupt
//! session never wipes settings. Load-with-fallback mirrors U12: a corrupt file
//! is backed up and defaults are used. No settings GUI — file plus defaults.
//!
//! Persistence is **sparse** (Electron-shell migration follow-up, 2026-08-12):
//! every write prunes entries equal to their `Config::default()` value, at
//! every nesting level, so the file records only the user's divergences and
//! absent keys keep tracking the binary's defaults across upgrades. (The old
//! full-struct write froze *every* default at last-save time — a default
//! change could never reach an installed config again.) Safe because the
//! whole schema loads partial objects: `Config` and each nested struct carry
//! `#[serde(default)]`.

mod schema;

pub use schema::{AutomationDefaults, Config, FeedConfig, ReasonEffectsConfig, Renderer, SubstrateKind};

use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Live settings, loaded once at startup. Other units read via [`get`].
pub struct ConfigStore {
    config: RwLock<Config>,
    path: PathBuf,
}

impl ConfigStore {
    /// Load from `path`, falling back to defaults (backing up a corrupt file).
    pub fn load(path: PathBuf) -> Self {
        let config = load_with_fallback(&path);
        Self {
            config: RwLock::new(config),
            path,
        }
    }

    /// In-memory store backed by `config`, touching no file (empty path). The
    /// [`crate::automations::AutomationManager`]'s pre-injection default and a
    /// convenient test seam — the real app injects a file-backed store.
    pub fn ephemeral(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            path: PathBuf::new(),
        }
    }

    pub fn get(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    /// Replace the live config and flush it to disk atomically. The on-disk
    /// write happens *first*: only after it succeeds is the in-memory copy
    /// swapped, so a failed flush (ENOSPC, dir perms) leaves the running config
    /// and the file it mirrors both unchanged. Returns the error string on
    /// failure so the caller (the settings menu) can surface it.
    pub fn set(&self, config: Config) -> Result<(), String> {
        write_atomic(&self.path, &config).map_err(|e| e.to_string())?;
        *self.config.write().unwrap() = config;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Serialize + atomically persist `config` to `path`: write a sibling temp file
/// then rename over the target, so a concurrent reader never observes a
/// half-written file. Creates the parent directory if it is absent (a
/// first-ever settings write, before any config file exists). Mirrors the
/// temp+rename discipline used by the session and resume stores.
///
/// The file is `0600` **on every write**, not only at token mint
/// (feed-pending-question U5, R10): `config.json` carries the feed bearer
/// token, and a rename replaces the inode — so a mint-time-only chmod would be
/// clobbered by the next unrelated settings write. On unix the temp file is
/// created `0600` *before any token bytes are written* (via `OpenOptions` +
/// `mode`, not a write-then-chmod that would leave a world-readable window),
/// and the mode is re-forced before the write to cover a `0644` leftover temp
/// a prior crash may have left for `create(true)` to reuse — so the
/// token-bearing bytes are never readable by another local user, not even
/// transiently (matches the `0600` automations store). This narrows only the
/// cross-user threat: a same-uid process can still read the file, so the
/// token-holder-equals-user trust model is unchanged.
fn write_atomic(path: &Path, config: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(&sparse_value(config))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        // A reused leftover temp keeps its old (possibly 0644) mode on open —
        // force 0600 while it is still empty, before the token bytes land.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The sparse on-disk form of `config`: the full struct serialized, then every
/// entry equal to its `Config::default()` counterpart pruned recursively (an
/// object emptied by pruning is dropped whole). Arrays and scalars compare
/// wholesale — a diverged array is kept in full, never element-diffed, because
/// element positions carry no stable identity. The result loads back to
/// exactly `config` through the schema's pervasive `#[serde(default)]`.
fn sparse_value(config: &Config) -> serde_json::Value {
    let mut value = serde_json::to_value(config).unwrap_or(serde_json::Value::Null);
    let default = serde_json::to_value(Config::default()).unwrap_or(serde_json::Value::Null);
    if prune_equal(&mut value, &default) {
        // Fully default (incl. equal-at-the-top, which mutates nothing): an
        // empty object, not the untouched full dump.
        return serde_json::Value::Object(serde_json::Map::new());
    }
    value
}

/// Recursively remove object entries equal to `default`'s. Returns whether
/// `value` ended up contributing nothing (equal, or an emptied object) so the
/// parent can drop the key.
fn prune_equal(value: &mut serde_json::Value, default: &serde_json::Value) -> bool {
    if value == default {
        return true;
    }
    if let (serde_json::Value::Object(map), serde_json::Value::Object(dmap)) = (value, default) {
        map.retain(|key, entry| match dmap.get(key) {
            Some(dentry) => !prune_equal(entry, dentry),
            // A key with no default counterpart (newer file than binary —
            // shouldn't happen, but never destroy data) is kept as-is.
            None => true,
        });
        return map.is_empty();
    }
    false
}

/// Default config file location: `$XDG_CONFIG_HOME/<app>/config.json`, where
/// `<app>` is [`crate::app_dir_name`] (`fly`, or a `FLY_APP_NAME` override so a
/// dev flavor keeps its own settings).
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(crate::app_dir_name()).join("config.json")
}

/// Read + parse, or fall back to defaults. A corrupt file is renamed aside so
/// it isn't silently overwritten and the user can recover it.
pub fn load_with_fallback(path: &Path) -> Config {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Config::default(), // missing → defaults
    };
    match serde_json::from_slice::<Config>(&bytes) {
        Ok(config) => config,
        Err(_) => {
            let backup = PathBuf::from(format!("{}.corrupt.bak", path.display()));
            let _ = std::fs::rename(path, &backup);
            Config::default()
        }
    }
}

/// Ensure the feed has a bearer token, minting + persisting one on first run
/// (feat-agent-state-local-feed, U4). A `127.0.0.1` listener is reachable by any
/// local process, so the token is the boundary — it must exist before the feed
/// server binds. Returns the live token (existing or freshly minted), or `None`
/// only if the mint could not be persisted (the caller then skips the feed
/// rather than serving with an unsaved secret the consumer can't learn).
///
/// A 256-bit CSPRNG hex token, matching the hook channel's strength
/// (`hooks::token`). Never logged.
pub fn ensure_feed_token(store: &ConfigStore) -> Option<String> {
    let cfg = store.get();
    if let Some(tok) = cfg.feed.token.clone() {
        return Some(tok);
    }
    let token = mint_feed_token();
    let mut next = cfg;
    next.feed.token = Some(token.clone());
    match store.set(next) {
        Ok(()) => Some(token),
        // A failed flush (ENOSPC, perms) leaves config unchanged; don't serve a
        // token the consumer can never read back from disk.
        Err(_) => None,
    }
}

/// A 256-bit CSPRNG token as lowercase hex — same strength/shape as the hook
/// per-pane tokens.
fn mint_feed_token() -> String {
    use rand::RngCore;
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    use std::fmt::Write;
    let mut s = String::with_capacity(raw.len() * 2);
    for b in raw {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Command: the frontend reads settings (leader key, renderer, …) from here.
#[tauri::command]
pub fn get_config(store: tauri::State<'_, std::sync::Arc<ConfigStore>>) -> Config {
    store.get()
}

/// Command: the settings menu writes settings back here. Persists atomically and
/// updates the live config so subsequent [`get_config`] calls (and the running
/// app) observe the change without a restart. Returns the stored config so the
/// frontend cache can sync to exactly what landed on disk.
#[tauri::command]
pub fn set_config(
    store: tauri::State<'_, std::sync::Arc<ConfigStore>>,
    config: Config,
) -> Result<Config, String> {
    store.set(config.clone())?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_persists_to_disk_and_updates_live_config() {
        let dir = tempfile::tempdir().unwrap();
        // Parent dir intentionally absent — `set` must create it on first write.
        let path = dir.path().join("nested").join("config.json");
        let store = ConfigStore::load(path.clone());

        let mut cfg = store.get();
        cfg.show_notifications_icon = false;
        cfg.font_size = 21;
        store.set(cfg).unwrap();

        // Live copy reflects the change immediately (no restart).
        assert!(!store.get().show_notifications_icon);
        assert_eq!(store.get().font_size, 21);

        // And it round-trips from disk.
        let reloaded = load_with_fallback(&path);
        assert!(!reloaded.show_notifications_icon);
        assert_eq!(reloaded.font_size, 21);
    }

    #[test]
    fn ensure_feed_token_mints_once_and_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = ConfigStore::load(path.clone());

        // No token initially; ensure mints + persists one.
        assert_eq!(store.get().feed.token, None);
        let first = ensure_feed_token(&store).unwrap();
        assert!(!first.is_empty());
        assert_eq!(first.len(), 64, "256-bit hex");

        // A second call returns the same token (not re-minted).
        assert_eq!(ensure_feed_token(&store).as_deref(), Some(first.as_str()));

        // And it survives a reload from disk (persisted, not just in-memory).
        let reloaded = ConfigStore::load(path);
        assert_eq!(reloaded.get().feed.token.as_deref(), Some(first.as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn config_file_stays_0600_after_a_second_unrelated_write() {
        // R10's regression shape: write_atomic replaces the inode on every
        // set(), so a mint-time-only chmod would be clobbered by the NEXT
        // unrelated settings write. The mode must hold durably.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = ConfigStore::load(path.clone());

        ensure_feed_token(&store).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "after mint");

        // A second, unrelated settings write — the one that used to clobber.
        let mut cfg = store.get();
        cfg.font_size = 19;
        store.set(cfg).unwrap();
        assert_eq!(mode(&path), 0o600, "after an unrelated rewrite");
        // The token itself survived the rewrite.
        assert!(store.get().feed.token.is_some());
    }

    #[test]
    fn writes_are_sparse_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = ConfigStore::load(path.clone());

        let mut cfg = store.get();
        cfg.font_size = 21; // top-level divergence
        cfg.reason_effects.finished.desktop = false; // nested divergence
        cfg.substrate = SubstrateKind::Tmux; // enum divergence
        store.set(cfg.clone()).unwrap();

        // The file holds ONLY the divergences…
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let obj = raw.as_object().unwrap();
        assert_eq!(
            obj.keys().collect::<Vec<_>>(),
            vec!["fontSize", "reasonEffects", "substrate"],
            "defaults must not be pinned"
        );
        // …pruned at every level: finished keeps desktop, drops sound/record;
        // the four untouched reasons vanish entirely.
        assert_eq!(
            raw["reasonEffects"],
            serde_json::json!({ "finished": { "desktop": false } })
        );

        // And it loads back to exactly the full config that was set.
        assert_eq!(load_with_fallback(&path), cfg);
    }

    #[test]
    fn a_setting_returned_to_its_default_disappears_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = ConfigStore::load(path.clone());

        let mut cfg = store.get();
        cfg.font_size = 21;
        store.set(cfg).unwrap();
        let mut cfg = store.get();
        cfg.font_size = Config::default().font_size; // user reverts
        store.set(cfg).unwrap();

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw,
            serde_json::json!({}),
            "a reverted setting must resume tracking the binary default"
        );
    }

    #[test]
    fn set_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = ConfigStore::load(path.clone());
        store.set(store.get()).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }
}
