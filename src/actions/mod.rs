//! Action dispatch. Each action is a TOML table with an `action` key.
//!
//! Built-in actions:
//! - `obs.scene`        — Settings: `scene` (string), `collection` (optional string)
//! - `obs.virtual_cam`  — toggle virtual cam
//! - `obs.input_volume` — Settings: `input` (source name), `step_db` (f32, default 1)
//!   Relative dB per encoder tick; alias `obs.volume`
//! - `system.volume`    — Settings: `step` (i32)
//! - `hotkey`           — Settings: `keys` (array of strings like ["Ctrl","Shift","M"])
//! - `page.next` / `page.prev` / `page.goto` (name)
//! - `voicemeeter.gain` — Settings: `target` (Strip|Bus), `index` (u32), `step` (f32)
//! - `voicemeeter.mute` — Settings: `target` (Strip|Bus), `index` (u32)

use std::{
    sync::Arc,
    time::Instant,
};

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
///
/// Wrapped in `Arc` by the dispatch loop so every incoming input event can
/// spawn its own tokio task without contending on the loop itself. Per-backend
/// `Mutex`es continue to serialise access to clients/state that aren't safe to
/// hit in parallel (OBS client, Voicemeeter DLL, hotkey sender).
pub struct ActionContext {
    pub state: Arc<StateInner>,
    pub obs: Arc<Mutex<Option<obs::ObsClient>>>,
    /// Instant of the last failed OBS connection attempt. Used to short-circuit
    /// repeated connect tries so rapid button presses while OBS is offline
    /// don't pile up 2-second timeouts on background tasks.
    pub obs_backoff: Mutex<Option<Instant>>,
    /// Remote handle is `!Send`; async code only holds `Arc<std::sync::Mutex<_>>`.
    #[cfg(windows)]
    pub vm: std::sync::Arc<std::sync::Mutex<Option<::voicemeeter::VoicemeeterRemote>>>,
    #[cfg(not(windows))]
    pub vm: Mutex<Option<()>>,
    pub volume: system_volume::VolumeBackend,
    pub hotkey: Mutex<hotkey::HotkeyBackend>,
}

impl ActionContext {
    pub async fn new(state: Arc<StateInner>) -> Self {
        Self {
            state,
            obs: Arc::new(Mutex::new(None)),
            obs_backoff: Mutex::new(None),
            #[cfg(windows)]
            vm: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(not(windows))]
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
        "obs.input_volume" | "obs.volume" => obs::input_volume(ctx, &d.action, d.rotation).await,
        "system.volume" => system_volume::volume(ctx, &d.action, d.rotation).await,
        "hotkey" => hotkey::send_hotkey(ctx, &d.action).await,
        "page.next" => page::cycle(ctx, 1).await,
        "page.prev" => page::cycle(ctx, -1).await,
        // "page.cycle" on an `on_rotate` binding uses the rotation sign.
        "page.cycle" => {
            let offset = d.rotation.map(|v| v.signum() as isize).unwrap_or(1);
            page::cycle(ctx, offset).await
        }
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
