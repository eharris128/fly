//! Configuration substrate (U13): a typed settings store under the XDG config
//! dir, a *separate* file from the disposable session state (U12), so a corrupt
//! session never wipes settings. Load-with-fallback mirrors U12: a corrupt file
//! is backed up and defaults are used. No settings GUI — file plus defaults.

mod schema;

pub use schema::{Config, Renderer};

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

    pub fn get(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
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

/// Command: the frontend reads settings (leader key, renderer, …) from here.
#[tauri::command]
pub fn get_config(store: tauri::State<'_, std::sync::Arc<ConfigStore>>) -> Config {
    store.get()
}
