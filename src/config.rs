//! Config model + hot reload.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::state::AppHandle;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub device: DeviceSection,
    pub obs: Option<ObsConfig>,
    pub voicemeeter: Option<VoicemeeterConfig>,
    #[serde(default)]
    pub pages: Vec<Page>,
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSection {
    pub brightness: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObsConfig {
    pub url: String,
    pub password: Option<String>,
    pub password_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VoicemeeterConfig {
    #[serde(default = "default_vm_flavor")]
    pub flavor: String,
}

fn default_vm_flavor() -> String {
    "potato".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub name: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub slots: BTreeMap<String, Slot>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub image: Option<String>,
    pub image_on: Option<String>,
    pub on_press: Option<ActionSpec>,
    pub on_release: Option<ActionSpec>,
    pub on_rotate: Option<ActionSpec>,
}

/// Deliberately untyped so we can parse a freeform action table with custom args.
pub type ActionSpec = toml::Table;

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&text).with_context(|| "parse config TOML")?;
    cfg.base_dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<()> {
    if cfg.pages.is_empty() {
        return Err(anyhow!("config has no [[pages]]"));
    }
    let names: Vec<&str> = cfg.pages.iter().map(|p| p.name.as_str()).collect();
    for (i, n) in names.iter().enumerate() {
        if names.iter().skip(i + 1).any(|m| m == n) {
            return Err(anyhow!("duplicate page name: {n}"));
        }
    }
    let defaults = cfg.pages.iter().filter(|p| p.default).count();
    if defaults > 1 {
        return Err(anyhow!("more than one page has default=true"));
    }
    Ok(())
}

pub fn resolve_asset(cfg: &Config, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cfg.base_dir.join(p)
    }
}

pub async fn watch(path: PathBuf, handle: AppHandle) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            warn!("failed to create file watcher: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        warn!("failed to watch config: {e}");
        return;
    }
    // Debounce a little: apply after we've had quiet for 200ms.
    let mut pending = false;
    loop {
        tokio::select! {
            Some(ev) = rx.recv() => {
                if matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    pending = true;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)), if pending => {
                pending = false;
                match load(&path) {
                    Ok(new_cfg) => {
                        info!("Reloading config");
                        handle.replace_config(Arc::new(new_cfg)).await;
                    }
                    Err(e) => warn!("config reload failed: {e}"),
                }
            }
        }
    }
}
