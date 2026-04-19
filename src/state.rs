//! Runtime state: active page, slot 2-state overlays, dispatcher.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use image::DynamicImage;
use mirajazz::device::Device;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::{
    actions::{self, ActionContext, Dispatch},
    config::{Config, Slot, resolve_asset},
    mapping::{InputEvent, SlotId},
    render,
    util::lock_sync,
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
    /// Last known Voicemeeter mute flags from poll + local toggles (strip/bus).
    pub vm_mute_latch: Mutex<HashMap<(String, u32), bool>>,
    pub device: Arc<Device>,
    pub reload_tx: mpsc::UnboundedSender<Reload>,
    /// One background redraw task per displayed slot — covers GIF playback,
    /// clocks, and system / voicemeeter volume meters. Inserting for a slot
    /// aborts any previous task for the same slot.
    pub slot_tasks: std::sync::Mutex<HashMap<SlotId, tokio::task::JoinHandle<()>>>,
    /// Bumped at the start of every full re-render so GIF loops stop before HID
    /// writes even when `abort`/`await` does not preempt an in-flight transfer.
    pub gif_display_epoch: AtomicU64,
    /// Wired after [`AppState`] construction so rendering can use the shared Voicemeeter client.
    pub action_ctx: StdMutex<Option<std::sync::Weak<ActionContext>>>,
    /// Voicemeeter mute poll task is started once after the first successful Remote init.
    pub vm_mute_poll_started: AtomicBool,
    /// Wakes volume meter background tasks right after encoder-driven level changes.
    pub volume_meter_wake: Notify,
}

impl StateInner {
    /// Cancel every per-slot redraw task without awaiting. Safe fallback for
    /// `Drop`, where we cannot run async code.
    fn abort_all_slot_tasks_best_effort(&self) {
        let handles: Vec<_> = lock_sync(&self.slot_tasks)
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in handles {
            h.abort();
        }
    }
}

impl Drop for StateInner {
    fn drop(&mut self) {
        self.abort_all_slot_tasks_best_effort();
    }
}

pub enum Reload {
    Config,
}

pub struct AppState {
    handle: AppHandle,
    reload_rx: Mutex<Option<mpsc::UnboundedReceiver<Reload>>>,
    ctx: Arc<ActionContext>,
}

impl AppState {
    pub async fn new(cfg: Config, device: Device) -> Result<Self> {
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
            vm_mute_latch: Mutex::new(HashMap::new()),
            device: Arc::new(device),
            reload_tx,
            slot_tasks: std::sync::Mutex::new(HashMap::new()),
            gif_display_epoch: AtomicU64::new(0),
            action_ctx: StdMutex::new(None),
            vm_mute_poll_started: AtomicBool::new(false),
            volume_meter_wake: Notify::new(),
        });

        let ctx = Arc::new(ActionContext::new(inner.clone()).await);
        if let Ok(mut slot) = inner.action_ctx.lock() {
            *slot = Some(Arc::downgrade(&ctx));
        }

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
                    // Resolve the binding synchronously (cheap - arc_swap +
                    // HashMap lookup + clone), then hand the rest off to a
                    // detached task. That way a slow backend - e.g. OBS
                    // waiting on a TCP timeout - can't stall the main
                    // dispatch loop from receiving further device input.
                    let Some((slot_id, spec, rotate)) = self.resolve_event(&ev).await else {
                        continue;
                    };
                    debug!("dispatch slot={slot_id:?} action={spec:?} rotate={rotate:?}");
                    let ctx = self.ctx.clone();
                    tokio::spawn(async move {
                        let d = Dispatch { slot: slot_id, action: spec, rotation: rotate };
                        if let Err(e) = actions::dispatch(&ctx, d).await {
                            warn!("action dispatch error: {e:?}");
                        }
                    });
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

    /// Look up the action bound to an input event on the current page. Runs
    /// on the dispatch loop's task so it stays fast and the config snapshot
    /// is captured atomically per-event.
    async fn resolve_event(
        &self,
        ev: &InputEvent,
    ) -> Option<(SlotId, crate::config::ActionSpec, Option<i32>)> {
        let cfg = self.handle.inner.config.load_full();
        let page_name = self.handle.inner.current_page.lock().await.clone();
        let page = cfg.pages.iter().find(|p| p.name == page_name)?;
        let (slot_id, action) = match ev {
            InputEvent::Press(id) => (*id, slot_for(page, *id)?.on_press.clone()),
            InputEvent::Release(id) => (*id, slot_for(page, *id)?.on_release.clone()),
            InputEvent::Rotate(id, _) => (*id, slot_for(page, *id)?.on_rotate.clone()),
        };
        let spec = action?;
        let rotate = if let InputEvent::Rotate(_, v) = ev { Some(*v as i32) } else { None };
        Some((slot_id, spec, rotate))
    }
}

fn slot_for(page: &crate::config::Page, slot: SlotId) -> Option<&Slot> {
    page.slots.get(&slot.to_config_key())
}

fn choose_slot_image(slot: &Slot, state: u8) -> Option<String> {
    if state >= 1 {
        slot.image_on.clone().or_else(|| slot.image.clone())
    } else {
        slot.image.clone()
    }
}

/// What to display in a single slot for the current (config, state) pair.
///
/// Produced by [`classify_slot`]; consumed by [`AppHandle::paint_slot`]. The
/// split lets the full-page and overlay render paths share almost everything,
/// with the overlay skipping only the "live" variants (clock / meters) that
/// already have a running periodic task.
enum SlotRender {
    Clock,
    SysMeter,
    #[cfg(windows)]
    VmMeter {
        target: crate::actions::voicemeeter::VmTarget,
        label: String,
    },
    #[cfg(not(windows))]
    VmMeterStub,
    /// Voicemeeter target string couldn't be parsed; surface a red tile and log.
    VmInvalid(String),
    Static(DynamicImage),
    Gif {
        frames: Vec<DynamicImage>,
        delays_ms: Vec<u32>,
    },
    /// No image configured — clear the slot.
    Clear,
}

#[cfg(debug_assertions)]
fn slot_render_name(kind: &SlotRender) -> &'static str {
    match kind {
        SlotRender::Clock => "Clock",
        SlotRender::SysMeter => "SysMeter",
        #[cfg(windows)]
        SlotRender::VmMeter { .. } => "VmMeter",
        #[cfg(not(windows))]
        SlotRender::VmMeterStub => "VmMeterStub",
        SlotRender::VmInvalid(_) => "VmInvalid",
        SlotRender::Static(_) => "Static",
        SlotRender::Gif { .. } => "Gif",
        SlotRender::Clear => "Clear",
    }
}

#[cfg_attr(not(windows), allow(unused_variables))]
fn classify_slot(cfg: &Config, slot_cfg: &Slot, state: u8) -> SlotRender {
    if slot_cfg.clock {
        return SlotRender::Clock;
    }
    if let Some(vm_disp) = &slot_cfg.volume_display_voicemeeter {
        #[cfg(windows)]
        {
            return match crate::actions::voicemeeter::VmTarget::parse(
                &vm_disp.target,
                vm_disp.index,
            ) {
                Ok(target) => {
                    let label = target.label();
                    SlotRender::VmMeter { target, label }
                }
                Err(e) => SlotRender::VmInvalid(format!("{e}")),
            };
        }
        #[cfg(not(windows))]
        {
            let _ = vm_disp;
            return SlotRender::VmMeterStub;
        }
    }
    if slot_cfg.volume_display_system {
        return SlotRender::SysMeter;
    }
    let Some(rel) = choose_slot_image(slot_cfg, state) else {
        return SlotRender::Clear;
    };
    let path = resolve_asset(cfg, &rel);
    match render::load_key_visual(&path) {
        Ok(render::KeyImage::Static(img)) => SlotRender::Static(img),
        Ok(render::KeyImage::Animated { frames, delays_ms }) => {
            if frames.is_empty() {
                SlotRender::Clear
            } else {
                SlotRender::Gif { frames, delays_ms }
            }
        }
        Err(e) => {
            warn!("load image {}: {:#}", path.display(), e);
            SlotRender::Clear
        }
    }
}

impl AppHandle {
    pub fn configured_brightness(&self) -> u8 {
        self.inner.config.load_full().device.brightness.unwrap_or(60)
    }

    pub(crate) fn action_context(&self) -> Option<Arc<ActionContext>> {
        let guard = self.inner.action_ctx.lock().ok()?;
        guard.as_ref()?.upgrade()
    }

    /// Stop GIF loops and wait until they cannot send another frame (avoids stale
    /// frames after `clear_all_button_images` / page change).
    /// Abort every slot redraw task (GIF/clock/meter) and wait until they no
    /// longer send device frames — avoids stale frames after a full re-render
    /// or teardown.
    pub async fn shutdown_gif_tasks(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = lock_sync(&self.inner.slot_tasks)
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in handles {
            h.abort();
            let _ = h.await;
        }
    }

    pub async fn replace_config(&self, new_cfg: Arc<Config>) {
        self.inner.config.store(new_cfg);
        let _ = self.inner.reload_tx.send(Reload::Config);
    }

    /// Push new `image` / `image_on` art for these slots only (no full clear, no clock/volume restart).
    /// Animated `image` / `image_on` starts a per-slot GIF loop (e.g. on-state `image_on` GIF).
    async fn redraw_slots_overlay_only(&self, slots: &[SlotId]) -> Result<()> {
        if slots.is_empty() {
            return Ok(());
        }
        let cfg = self.inner.config.load_full();
        let page_name = self.inner.current_page.lock().await.clone();
        let Some(page) = cfg.pages.iter().find(|p| p.name == page_name) else {
            return Ok(());
        };
        let states = self.inner.slot_states.lock().await.clone();
        // Reuse the current epoch: overlay doesn't invalidate other slots' running tasks,
        // it only aborts+replaces tasks on the slots it's redrawing (via `slot_tasks`).
        let this_epoch = self.inner.gif_display_epoch.load(Ordering::Acquire);

        for &slot_id in slots {
            let Some(image_idx) = slot_id.image_index() else {
                continue;
            };
            let Some(slot_cfg) = page.slots.get(&slot_id.to_config_key()) else {
                continue;
            };
            let kind = classify_slot(&cfg, slot_cfg, *states.get(&slot_id).unwrap_or(&0));
            self.paint_slot(slot_id, image_idx, kind, this_epoch, false).await;
        }
        self.inner.device.flush().await.ok();
        Ok(())
    }

    pub async fn render_current_page(&self) -> Result<()> {
        // Invalidate in-flight GIF loops *before* join: they must observe a new
        // epoch and bail out without calling `set_button_image` for old content.
        let prev_epoch = self.inner.gif_display_epoch.fetch_add(1, Ordering::AcqRel);
        let this_epoch = prev_epoch.wrapping_add(1);

        self.shutdown_gif_tasks().await;

        let cfg = self.inner.config.load_full();
        let page_name = self.inner.current_page.lock().await.clone();
        let page = cfg
            .pages
            .iter()
            .find(|p| p.name == page_name)
            .context("current page not found")?;

        let states = self.inner.slot_states.lock().await.clone();
        self.inner.device.clear_all_button_images().await.ok();
        self.inner.device.flush().await.ok();

        for slot_id in SlotId::all_displayed() {
            let Some(image_idx) = slot_id.image_index() else { continue };
            let Some(slot_cfg) = page.slots.get(&slot_id.to_config_key()) else {
                let _ = self.inner.device.clear_button_image(image_idx).await;
                continue;
            };
            let kind = classify_slot(&cfg, slot_cfg, *states.get(&slot_id).unwrap_or(&0));
            self.paint_slot(slot_id, image_idx, kind, this_epoch, true).await;
        }
        self.inner.device.flush().await.ok();
        Ok(())
    }

    /// Apply a [`SlotRender`] decision to the device. The two render paths
    /// (full page + overlay) share this; `allow_live=false` skips clock/meter
    /// variants (their background tasks are already running and must not be
    /// restarted on overlay redraws).
    async fn paint_slot(
        &self,
        slot_id: SlotId,
        image_idx: u8,
        kind: SlotRender,
        this_epoch: u64,
        allow_live: bool,
    ) {
        let fmt = render::key_format();
        match kind {
            SlotRender::Clock if allow_live => {
                let img = render::render_clock_image(chrono::Local::now());
                if let Err(e) = self.inner.device.set_button_image(image_idx, fmt, img).await {
                    warn!("set_button_image clock {slot_id:?} (idx={image_idx}): {e}");
                }
                self.spawn_refresh_task(
                    slot_id,
                    image_idx,
                    Duration::from_secs(1),
                    false,
                    this_epoch,
                    move || async move {
                        Some(render::render_clock_image(chrono::Local::now()))
                    },
                );
            }
            #[cfg(windows)]
            SlotRender::VmMeter { target, label } if allow_live => {
                let Some(actx) = self.action_context() else {
                    warn!("voicemeeter volume display: action context not wired for {slot_id:?}");
                    let img = render::solid_tile([40u8, 12u8, 12u8]);
                    let _ = self.inner.device.set_button_image(image_idx, fmt, img).await;
                    return;
                };
                let holder = actx.vm.clone();
                let (db0, created0) = match tokio::task::spawn_blocking({
                    let h = holder.clone();
                    let t = target.clone();
                    move || crate::actions::voicemeeter::vm_init_and_read_gain_db(&h, &t)
                })
                .await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        warn!("voicemeeter volume display read failed for {slot_id:?}: {e}");
                        let img = render::solid_tile([40u8, 12u8, 12u8]);
                        let _ = self.inner.device.set_button_image(image_idx, fmt, img).await;
                        return;
                    }
                    Err(e) => {
                        warn!("voicemeeter volume display join error {slot_id:?}: {e}");
                        let img = render::solid_tile([40u8, 12u8, 12u8]);
                        let _ = self.inner.device.set_button_image(image_idx, fmt, img).await;
                        return;
                    }
                };
                crate::actions::voicemeeter::try_spawn_vm_mute_poll_after_init(
                    actx.as_ref(),
                    created0,
                );
                let img0 = render::render_voicemeeter_gain_meter(db0, &label);
                if let Err(e) = self
                    .inner
                    .device
                    .set_button_image(image_idx, fmt, img0)
                    .await
                {
                    warn!("set_button_image vm volume {slot_id:?} (idx={image_idx}): {e}");
                }
                self.spawn_refresh_task(
                    slot_id,
                    image_idx,
                    Duration::from_millis(100),
                    true,
                    this_epoch,
                    move || {
                        let h = holder.clone();
                        let t = target.clone();
                        let label = label.clone();
                        async move {
                            let db = tokio::task::spawn_blocking(move || {
                                crate::actions::voicemeeter::vm_init_and_read_gain_db(&h, &t)
                                    .map(|(d, _)| d)
                            })
                            .await
                            .ok()?
                            .ok()?;
                            Some(render::render_voicemeeter_gain_meter(db, &label))
                        }
                    },
                );
            }
            #[cfg(not(windows))]
            SlotRender::VmMeterStub if allow_live => {
                let img = render::render_volume_stub("NO VM");
                if let Err(e) = self.inner.device.set_button_image(image_idx, fmt, img).await {
                    warn!("set_button_image vm volume stub {slot_id:?} (idx={image_idx}): {e}");
                }
            }
            SlotRender::VmInvalid(err) if allow_live => {
                warn!("voicemeeter volume display target invalid for {slot_id:?}: {err}");
                let img = render::solid_tile([40u8, 12u8, 12u8]);
                let _ = self.inner.device.set_button_image(image_idx, fmt, img).await;
            }
            SlotRender::SysMeter if allow_live => {
                let meter = crate::actions::system_volume::VolumeBackend::new();
                let level0 = tokio::task::spawn_blocking({
                    let m = meter.clone();
                    move || m.master_scalar().unwrap_or(0.0)
                })
                .await
                .unwrap_or(0.0);
                let img0 = render::render_system_volume_meter(level0);
                if let Err(e) = self.inner.device.set_button_image(image_idx, fmt, img0).await {
                    warn!("set_button_image system volume {slot_id:?} (idx={image_idx}): {e}");
                }
                self.spawn_refresh_task(
                    slot_id,
                    image_idx,
                    Duration::from_millis(100),
                    true,
                    this_epoch,
                    move || {
                        let m = meter.clone();
                        async move {
                            let level = tokio::task::spawn_blocking(move || {
                                m.master_scalar().unwrap_or(0.0)
                            })
                            .await
                            .ok()?;
                            Some(render::render_system_volume_meter(level))
                        }
                    },
                );
            }
            SlotRender::Static(img) => {
                if let Some(h) = lock_sync(&self.inner.slot_tasks).remove(&slot_id) {
                    h.abort();
                }
                if let Err(e) = self.inner.device.set_button_image(image_idx, fmt, img).await {
                    warn!("set_button_image {slot_id:?} (idx={image_idx}): {e}");
                }
            }
            SlotRender::Gif { frames, delays_ms } => {
                self.spawn_gif_task(slot_id, image_idx, frames, delays_ms, this_epoch);
            }
            SlotRender::Clear => {
                if let Some(h) = lock_sync(&self.inner.slot_tasks).remove(&slot_id) {
                    h.abort();
                }
                let _ = self.inner.device.clear_button_image(image_idx).await;
            }
            // Live kinds reach here only on overlay redraw (allow_live=false).
            // Their periodic task is already running; don't touch it. Today
            // only `sync_slot_kind` triggers overlay redraws, and it targets
            // action-bound slots (obs.scene / obs.virtual_cam / voicemeeter.mute)
            // that are never live. If that contract ever changes, the old
            // live task will keep running and overwrite the new art until the
            // next full re-render — log so it's noticed.
            #[cfg(debug_assertions)]
            other => {
                debug_assert!(
                    !allow_live,
                    "unreachable paint_slot branch on full redraw: kind={}",
                    slot_render_name(&other)
                );
                warn!(
                    "overlay redraw hit live-kind slot {slot_id:?} ({}); leaving running task intact",
                    slot_render_name(&other)
                );
            }
            #[cfg(not(debug_assertions))]
            _ => {}
        }
    }

    /// Spawn a GIF playback loop for `slot_id` and record it in
    /// [`StateInner::slot_tasks`] (aborting any prior task for the same slot).
    fn spawn_gif_task(
        &self,
        slot_id: SlotId,
        image_idx: u8,
        frames: Vec<DynamicImage>,
        delays_ms: Vec<u32>,
        this_epoch: u64,
    ) {
        if frames.is_empty() {
            if let Some(h) = lock_sync(&self.inner.slot_tasks).remove(&slot_id) {
                h.abort();
            }
            return;
        }
        let device = self.inner.device.clone();
        let epoch_holder = self.inner.clone();
        let fmt = render::key_format();
        let handle = tokio::spawn(async move {
            let mut i = 0usize;
            loop {
                if epoch_holder.gif_display_epoch.load(Ordering::Acquire) != this_epoch {
                    return;
                }
                let img = frames[i].clone();
                if device
                    .set_button_image(image_idx, fmt, img)
                    .await
                    .is_err()
                {
                    break;
                }
                if epoch_holder.gif_display_epoch.load(Ordering::Acquire) != this_epoch {
                    return;
                }
                let _ = device.flush().await;
                let delay = delays_ms.get(i).copied().unwrap_or(100).clamp(16, 10_000);
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                i = (i + 1) % frames.len();
            }
        });
        if let Some(old) = lock_sync(&self.inner.slot_tasks).insert(slot_id, handle) {
            old.abort();
        }
    }

    /// Spawn a generic periodic-redraw task for `slot_id`.
    ///
    /// `produce` is invoked on each interval tick (or when
    /// [`StateInner::volume_meter_wake`] fires when `use_volume_wake` is set);
    /// it can return `None` to skip a frame. The first tick fires after
    /// `interval` has elapsed — callers paint the initial frame synchronously
    /// before spawning this task, so firing immediately would just re-push
    /// the same bytes.
    fn spawn_refresh_task<F, Fut>(
        &self,
        slot_id: SlotId,
        image_idx: u8,
        interval: Duration,
        use_volume_wake: bool,
        this_epoch: u64,
        mut produce: F,
    ) where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Option<DynamicImage>> + Send,
    {
        let device = self.inner.device.clone();
        let epoch_holder = self.inner.clone();
        let fmt = render::key_format();
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(Instant::now() + interval, interval);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                if use_volume_wake {
                    tokio::select! {
                        biased;
                        _ = epoch_holder.volume_meter_wake.notified() => {},
                        _ = tick.tick() => {},
                    }
                } else {
                    tick.tick().await;
                }
                if epoch_holder.gif_display_epoch.load(Ordering::Acquire) != this_epoch {
                    return;
                }
                let Some(img) = produce().await else { continue };
                if device
                    .set_button_image(image_idx, fmt, img)
                    .await
                    .is_err()
                {
                    break;
                }
                if epoch_holder.gif_display_epoch.load(Ordering::Acquire) != this_epoch {
                    return;
                }
                let _ = device.flush().await;
            }
        });
        if let Some(old) = lock_sync(&self.inner.slot_tasks).insert(slot_id, handle) {
            old.abort();
        }
    }

    pub async fn set_slot_state(&self, slot: SlotId, state: u8) {
        let mut states = self.inner.slot_states.lock().await;
        if states.get(&slot).copied() == Some(state) {
            return;
        }
        states.insert(slot, state);
        drop(states);
        if let Err(e) = self.redraw_slots_overlay_only(&[slot]).await {
            warn!("redraw after set_slot_state: {e:?}");
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

    pub fn from_inner(inner: Arc<StateInner>) -> Self {
        Self { inner }
    }

    /// Apply Voicemeeter DLL mute flags into [`StateInner::vm_mute_latch`].
    ///
    /// The periodic poll updates button art via [`Self::sync_vm_mute`]; without this,
    /// the latch still holds the pre-UI value, so the next `voicemeeter.mute` press
    /// toggles from the wrong baseline (double-press after changing mute in Voicemeeter).
    pub async fn merge_vm_mute_latch_from_poll(&self, polled: &HashMap<(String, u32), bool>) {
        let mut latch = self.inner.vm_mute_latch.lock().await;
        for (k, v) in polled {
            latch.insert(k.clone(), *v);
        }
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
    ///
    /// Slot state matches the usual asset layout: `image` = muted / mic-off art,
    /// `image_on` = live / mic-on art (see `config.example.toml`).
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
                Some(true) => 0,  // muted → `image` (e.g. mic_off)
                Some(false) => 1, // live → `image_on` (e.g. mic_on)
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
        let mut changed_ids: Vec<SlotId> = Vec::new();
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
                changed_ids.push(id);
            }
        }
        drop(states);
        if !changed_ids.is_empty() {
            if let Err(e) = self.redraw_slots_overlay_only(&changed_ids).await {
                warn!("redraw after sync_slot_kind({action_name}): {e:?}");
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
        for slot in page.slots.values() {
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
