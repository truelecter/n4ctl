//! Action dispatch. Each action is a TOML table with an `action` key.
//!
//! Built-in actions:
//! - `obs.scene`        — Settings: `scene` (string), `collection` (optional string)
//! - `obs.virtual_cam`  — toggle virtual cam
//! - `system.volume`    — Settings: `step` (i32)
//! - `hotkey`           — Settings: `keys` (array of strings like ["Ctrl","Shift","M"])
//! - `page.next` / `page.prev` / `page.goto` (name)
//! - `voicemeeter.gain` — Settings: `target` (Strip|Bus), `index` (u32), `step` (f32)
//! - `voicemeeter.mute` — Settings: `target` (Strip|Bus), `index` (u32)

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::warn;

use crate::{
    config::ActionSpec,
    mapping::SlotId,
    state::StateInner,
};

pub mod hotkey;
pub mod obs;
pub mod page;
pub mod system_volume;
pub mod voicemeeter;

/// Parameters for a single dispatch call.
pub struct Dispatch {
    pub slot: SlotId,
    pub action: ActionSpec,
    pub rotation: Option<i32>,
}

/// Context that holds OBS/Voicemeeter/etc. clients and the AppHandle.
pub struct ActionContext {
    pub state: Arc<StateInner>,
    pub obs: Mutex<Option<obs::ObsClient>>,
    pub vm: Mutex<Option<voicemeeter::VoicemeeterClient>>,
    pub volume: system_volume::VolumeBackend,
    pub hotkey: Mutex<hotkey::HotkeyBackend>,
}

impl ActionContext {
    pub async fn new(state: Arc<StateInner>) -> Self {
        Self {
            state,
            obs: Mutex::new(None),
            vm: Mutex::new(None),
            volume: system_volume::VolumeBackend::new(),
            hotkey: Mutex::new(hotkey::HotkeyBackend::new()),
        }
    }

    pub fn handle(&self) -> crate::state::AppHandle {
        // Rebuild an AppHandle from the inner state we hold. Since AppHandle is a
        // thin wrapper around `Arc<Inner>`, this is cheap.
        crate::state::AppHandle::from_inner(self.state.clone())
    }
}

pub async fn dispatch(ctx: &ActionContext, d: Dispatch) -> Result<()> {
    let action_name = d
        .action
        .get("action")
        .and_then(|v| v.as_str())
        .context("action missing 'action' field")?
        .to_string();

    match action_name.as_str() {
        "obs.scene" => obs::scene(ctx, &d.action).await,
        "obs.virtual_cam" | "obs.virtualcam" => obs::virtual_cam(ctx, d.slot, &d.action).await,
        "system.volume" => system_volume::volume(ctx, &d.action, d.rotation).await,
        "hotkey" => hotkey::send_hotkey(ctx, &d.action).await,
        "page.next" => page::cycle(ctx, 1).await,
        "page.prev" => page::cycle(ctx, -1).await,
        "page.goto" => page::goto(ctx, &d.action).await,
        "voicemeeter.gain" => voicemeeter::gain(ctx, &d.action, d.rotation).await,
        "voicemeeter.mute" => voicemeeter::mute_toggle(ctx, &d.action).await,
        other => {
            warn!("unknown action: {}", other);
            Ok(())
        }
    }
}

/// Parse a string field out of a toml table with a helpful error.
pub fn str_field<'a>(table: &'a ActionSpec, name: &str) -> Result<&'a str> {
    table
        .get(name)
        .and_then(|v| v.as_str())
        .with_context(|| format!("action missing '{name}' string"))
}

pub fn i32_field(table: &ActionSpec, name: &str, default: i32) -> i32 {
    table
        .get(name)
        .and_then(|v| v.as_integer())
        .map(|n| n as i32)
        .unwrap_or(default)
}

pub fn f32_field(table: &ActionSpec, name: &str, default: f32) -> f32 {
    match table.get(name) {
        Some(v) => {
            if let Some(f) = v.as_float() {
                f as f32
            } else if let Some(i) = v.as_integer() {
                i as f32
            } else {
                default
            }
        }
        None => default,
    }
}

pub fn u32_field(table: &ActionSpec, name: &str, default: u32) -> u32 {
    table
        .get(name)
        .and_then(|v| v.as_integer())
        .map(|n| n as u32)
        .unwrap_or(default)
}
