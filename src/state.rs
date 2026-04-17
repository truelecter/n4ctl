//! Runtime state: active page, slot 2-state overlays, dispatcher.

use std::{
    collections::HashMap,
    sync::Arc,
};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use mirajazz::device::Device;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::{
    actions::{self, ActionContext, Dispatch},
    config::{Config, Slot, resolve_asset},
    mapping::{InputEvent, SlotId},
    render,
};

/// Shared handle passed to tasks that need to poke state (config reload).
#[derive(Clone)]
pub struct AppHandle {
    inner: Arc<StateInner>,
}

pub struct StateInner {
    pub config: ArcSwap<Config>,
    pub current_page: Mutex<String>,
    pub slot_states: Mutex<HashMap<SlotId, u8>>,
    pub device: Arc<Device>,
    pub reload_tx: mpsc::UnboundedSender<Reload>,
}

pub enum Reload {
    Config,
}

pub struct AppState {
    handle: AppHandle,
    reload_rx: Mutex<Option<mpsc::UnboundedReceiver<Reload>>>,
    ctx: ActionContext,
}

impl AppState {
    pub async fn new(cfg: Config, device: Device, _evt_tx: mpsc::UnboundedSender<InputEvent>) -> Result<Self> {
        let initial_page = cfg
            .pages
            .iter()
            .find(|p| p.default)
            .or_else(|| cfg.pages.first())
            .context("config has no pages")?
            .name
            .clone();

        let (reload_tx, reload_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(StateInner {
            config: ArcSwap::from_pointee(cfg),
            current_page: Mutex::new(initial_page),
            slot_states: Mutex::new(HashMap::new()),
            device: Arc::new(device),
            reload_tx,
        });

        let ctx = ActionContext::new(inner.clone()).await;

        Ok(Self {
            handle: AppHandle { inner },
            reload_rx: Mutex::new(Some(reload_rx)),
            ctx,
        })
    }

    pub fn device(&self) -> &Device {
        &self.handle.inner.device
    }

    pub fn device_arc(&self) -> Arc<Device> {
        self.handle.inner.device.clone()
    }

    pub fn clone_handle(&self) -> AppHandle {
        self.handle.clone()
    }

    pub async fn render_current_page(&self) -> Result<()> {
        self.handle.render_current_page().await
    }

    pub async fn run_dispatch_loop(&self, mut evt_rx: mpsc::UnboundedReceiver<InputEvent>) {
        let mut reload_rx = self.reload_rx.lock().await.take().expect("reload_rx taken");
        loop {
            tokio::select! {
                Some(ev) = evt_rx.recv() => {
                    if let Err(e) = self.handle_event(ev).await {
                        warn!("action dispatch error: {e:?}");
                    }
                }
                Some(Reload::Config) = reload_rx.recv() => {
                    info!("applying config reload");
                    if let Err(e) = self.handle.render_current_page().await {
                        warn!("re-render after reload failed: {e:?}");
                    }
                }
                else => break,
            }
        }
    }

    async fn handle_event(&self, ev: InputEvent) -> Result<()> {
        let cfg = self.handle.inner.config.load_full();
        let page_name = self.handle.inner.current_page.lock().await.clone();
        let page = match cfg.pages.iter().find(|p| p.name == page_name) {
            Some(p) => p,
            None => return Ok(()),
        };
        let (slot_id, action) = match &ev {
            InputEvent::Press(id) => {
                if let Some(slot) = slot_for(page, *id) {
                    (*id, slot.on_press.clone())
                } else {
                    return Ok(());
                }
            }
            InputEvent::Release(id) => {
                if let Some(slot) = slot_for(page, *id) {
                    (*id, slot.on_release.clone())
                } else {
                    return Ok(());
                }
            }
            InputEvent::Rotate(id, _) => {
                if let Some(slot) = slot_for(page, *id) {
                    (*id, slot.on_rotate.clone())
                } else {
                    return Ok(());
                }
            }
        };
        let Some(spec) = action else { return Ok(()); };
        let rotate = if let InputEvent::Rotate(_, v) = ev { Some(v as i32) } else { None };
        debug!("dispatch slot={slot_id:?} action={spec:?} rotate={rotate:?}");
        actions::dispatch(&self.ctx, Dispatch {
            slot: slot_id,
            action: spec,
            rotation: rotate,
        })
        .await
    }
}

fn slot_for(page: &crate::config::Page, slot: SlotId) -> Option<&Slot> {
    let key = slot_to_config_key(slot);
    page.slots.get(&key)
}

fn slot_to_config_key(slot: SlotId) -> String {
    match slot {
        SlotId::Key(n) => format!("key_{n}"),
        SlotId::Strip(n) => format!("strip_{n}"),
        SlotId::Knob(n) => format!("knob_{n}"),
    }
}

impl AppHandle {
    pub async fn replace_config(&self, new_cfg: Arc<Config>) {
        self.inner.config.store(new_cfg);
        let _ = self.inner.reload_tx.send(Reload::Config);
    }

    pub async fn render_current_page(&self) -> Result<()> {
        let cfg = self.inner.config.load_full();
        let page_name = self.inner.current_page.lock().await.clone();
        let page = cfg
            .pages
            .iter()
            .find(|p| p.name == page_name)
            .context("current page not found")?;

        let states = self.inner.slot_states.lock().await.clone();
        self.inner.device.clear_all_button_images().await.ok();

        for slot_id in SlotId::all_displayed() {
            let Some(image_idx) = slot_id.image_index() else { continue };
            let key = slot_to_config_key(slot_id);
            let image = page
                .slots
                .get(&key)
                .and_then(|slot| choose_slot_image(slot, *states.get(&slot_id).unwrap_or(&0)));
            let Some(rel) = image else { continue };
            let path = resolve_asset(&cfg, &rel);
            match render::load_key_image(&path) {
                Ok(img) => {
                    if let Err(e) = self
                        .inner
                        .device
                        .set_button_image(image_idx, render::key_format(), img)
                        .await
                    {
                        warn!("set_button_image {slot_id:?} (idx={image_idx}): {e}");
                    }
                }
                Err(e) => warn!("load image {}: {}", path.display(), e),
            }
        }
        self.inner.device.flush().await.ok();
        Ok(())
    }

    pub async fn set_slot_state(&self, slot: SlotId, state: u8) {
        let mut states = self.inner.slot_states.lock().await;
        states.insert(slot, state);
        drop(states);
        if let Err(e) = self.render_current_page().await {
            warn!("render after set_slot_state: {e:?}");
        }
    }

    pub async fn goto_page(&self, name: &str) -> Result<()> {
        let cfg = self.inner.config.load_full();
        if !cfg.pages.iter().any(|p| p.name == name) {
            return Err(anyhow::anyhow!("no such page: {name}"));
        }
        *self.inner.current_page.lock().await = name.to_string();
        self.render_current_page().await
    }

    pub async fn cycle_page(&self, offset: isize) -> Result<()> {
        let cfg = self.inner.config.load_full();
        if cfg.pages.is_empty() {
            return Ok(());
        }
        let cur = self.inner.current_page.lock().await.clone();
        let idx = cfg.pages.iter().position(|p| p.name == cur).unwrap_or(0) as isize;
        let n = cfg.pages.len() as isize;
        let new = ((idx + offset) % n + n) % n;
        let new_name = cfg.pages[new as usize].name.clone();
        *self.inner.current_page.lock().await = new_name;
        self.render_current_page().await
    }

    #[allow(dead_code)]
    pub fn config(&self) -> Arc<Config> {
        self.inner.config.load_full()
    }

    #[allow(dead_code)]
    pub fn device(&self) -> Arc<Device> {
        self.inner.device.clone()
    }
}

fn choose_slot_image(slot: &Slot, state: u8) -> Option<String> {
    if state >= 1 {
        slot.image_on.clone().or_else(|| slot.image.clone())
    } else {
        slot.image.clone()
    }
}

impl AppHandle {
    pub fn from_inner(inner: Arc<StateInner>) -> Self {
        Self { inner }
    }

    /// Update any slot on the current page whose `on_press` action is `obs.scene`
    /// so that only the slot for the active scene name shows its "on" image.
    pub async fn sync_obs_scene(&self, active: &str) {
        self.sync_slot_kind("obs.scene", |slot| {
            let want = slot
                .on_press
                .as_ref()
                .and_then(|a| a.get("scene"))
                .and_then(|v| v.as_str());
            if want == Some(active) { 1 } else { 0 }
        })
        .await;
    }

    /// Set the 2-state icon of every `obs.virtual_cam` slot to match `active`.
    pub async fn sync_virtual_cam(&self, active: bool) {
        self.sync_slot_kind("obs.virtual_cam", |_slot| if active { 1 } else { 0 }).await;
    }

    /// Update mute-button icons for `voicemeeter.mute` slots.
    /// `is_muted(target, index)` should return the current mute value.
    pub async fn sync_vm_mute<F>(&self, mut is_muted: F)
    where
        F: FnMut(&str, u32) -> Option<bool>,
    {
        self.sync_slot_kind("voicemeeter.mute", move |slot| {
            let spec = slot.on_press.as_ref();
            let target = spec
                .and_then(|a| a.get("target"))
                .and_then(|v| v.as_str())
                .unwrap_or("Strip");
            let index = spec
                .and_then(|a| a.get("index"))
                .and_then(|v| v.as_integer())
                .unwrap_or(0) as u32;
            match is_muted(target, index) {
                Some(true) => 1,
                Some(false) => 0,
                None => 0,
            }
        })
        .await;
    }

    async fn sync_slot_kind<F>(&self, action_name: &str, mut pick: F)
    where
        F: FnMut(&Slot) -> u8,
    {
        let cfg = self.inner.config.load_full();
        let page_name = self.inner.current_page.lock().await.clone();
        let Some(page) = cfg.pages.iter().find(|p| p.name == page_name) else {
            return;
        };
        let mut states = self.inner.slot_states.lock().await;
        let mut changed = false;
        for (key, slot) in &page.slots {
            let Some(id) = crate::mapping::SlotId::parse(key) else { continue; };
            let is_match = slot
                .on_press
                .as_ref()
                .and_then(|a| a.get("action"))
                .and_then(|v| v.as_str())
                == Some(action_name);
            if !is_match {
                continue;
            }
            let new_state = pick(slot);
            let existing = states.get(&id).copied().unwrap_or(0);
            if existing != new_state {
                states.insert(id, new_state);
                changed = true;
            }
        }
        drop(states);
        if changed {
            if let Err(e) = self.render_current_page().await {
                warn!("render after sync_slot_kind({action_name}): {e:?}");
            }
        }
    }

    /// Collect all (target, index) pairs used by voicemeeter.mute slots on the
    /// current page so a polling task knows what to read.
    pub async fn list_vm_mute_params(&self) -> Vec<(String, u32)> {
        let cfg = self.inner.config.load_full();
        let page_name = self.inner.current_page.lock().await.clone();
        let mut out = Vec::new();
        let Some(page) = cfg.pages.iter().find(|p| p.name == page_name) else {
            return out;
        };
        for (_key, slot) in &page.slots {
            let Some(spec) = slot.on_press.as_ref() else { continue };
            if spec.get("action").and_then(|v| v.as_str()) != Some("voicemeeter.mute") {
                continue;
            }
            let target = spec.get("target").and_then(|v| v.as_str()).unwrap_or("Strip").to_string();
            let index = spec.get("index").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
            out.push((target, index));
        }
        out
    }
}
