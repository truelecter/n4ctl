//! Voicemeeter via the [`voicemeeter`](https://docs.rs/voicemeeter) crate (Windows).

use crate::{
    actions::{ActionContext, f32_field, str_field, u32_field},
    config::ActionSpec,
};

use anyhow::{Result, anyhow};

#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

#[cfg(windows)]
use tracing::debug;

#[cfg(windows)]
use crate::util::lock_sync;

/// Typed Voicemeeter target: a strip or a bus at a given index.
///
/// Removes the case-insensitive `"strip"` / `"bus"` string-compare chain that
/// used to live in every read/write helper.
#[derive(Clone, Debug)]
pub enum VmTarget {
    Strip(u32),
    Bus(u32),
}

impl VmTarget {
    /// Parse `(target, index)` pairs from config/ActionSpec. Accepts
    /// `"Strip"` / `"strip"` / `"STRIP"` (and same for `"Bus"`).
    pub fn parse(target: &str, index: u32) -> Result<Self> {
        match target.to_ascii_lowercase().as_str() {
            "strip" => Ok(VmTarget::Strip(index)),
            "bus" => Ok(VmTarget::Bus(index)),
            other => Err(anyhow!(
                "voicemeeter target must be Strip or Bus, got '{other}'"
            )),
        }
    }

    /// Compact label for on-display meters: `"S0"` / `"B1"`.
    pub fn label(&self) -> String {
        match *self {
            VmTarget::Strip(i) => format!("S{i}"),
            VmTarget::Bus(i) => format!("B{i}"),
        }
    }

    /// `Strip[i].Gain` / `Bus[i].Gain` prefix for `VBVMR_SetParameters`.
    #[cfg(windows)]
    pub fn gain_script_prefix(&self) -> String {
        match *self {
            VmTarget::Strip(i) => format!("Strip[{i}].Gain"),
            VmTarget::Bus(i) => format!("Bus[{i}].Gain"),
        }
    }

    /// Bare mute read (caller must hold [`with_voicemeeter_io`]).
    #[cfg(windows)]
    pub fn mute_get(&self, vm: &::voicemeeter::VoicemeeterRemote) -> Option<bool> {
        match *self {
            VmTarget::Strip(i) => vm.parameters().strip(i as usize).ok()?.mute().get().ok(),
            VmTarget::Bus(i) => vm.parameters().bus(i as usize).ok()?.mute().get().ok(),
        }
    }

    /// Bare gain read in dB (caller must hold [`with_voicemeeter_io`]).
    #[cfg(windows)]
    pub fn gain_get(&self, vm: &::voicemeeter::VoicemeeterRemote) -> Option<f32> {
        match *self {
            VmTarget::Strip(i) => vm.parameters().strip(i as usize).ok()?.gain().get().ok(),
            VmTarget::Bus(i) => vm.parameters().bus(i as usize).ok()?.gain().get().ok(),
        }
    }

    /// Bare mute write (caller must hold [`with_voicemeeter_io`]).
    #[cfg(windows)]
    pub fn mute_set(&self, vm: &::voicemeeter::VoicemeeterRemote, muted: bool) -> Result<()> {
        match *self {
            VmTarget::Strip(i) => vm
                .parameters()
                .strip(i as usize)
                .map_err(|e| anyhow!("{e}"))?
                .mute()
                .set(muted)
                .map_err(|e| anyhow!("{e}")),
            VmTarget::Bus(i) => vm
                .parameters()
                .bus(i as usize)
                .map_err(|e| anyhow!("{e}"))?
                .mute()
                .set(muted)
                .map_err(|e| anyhow!("{e}")),
        }
    }
}

#[cfg(windows)]
fn voicemeeter_io_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

/// Serialize Voicemeeter Remote calls across `spawn_blocking` threads. The DLL expects
/// [`VoicemeeterRemote::is_parameters_dirty`] to be driven regularly so `get()` reflects
/// live state, and the crate documents single-thread use for that call.
#[cfg(windows)]
pub(crate) fn with_voicemeeter_io<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let _guard = lock_sync(voicemeeter_io_mutex());
    f()
}

#[cfg(windows)]
fn format_script_float(v: f32) -> String {
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Obtain a live [`VoicemeeterRemote`] handle (lazily initializing on first use),
/// run `f` under the DLL's serialization lock, and return its result alongside
/// a flag that's `true` if this call is the one that created the remote.
///
/// Call sites that only need the "created" signal ignore the `R` via `()`.
///
/// Recovery note: the cached remote's [`VoicemeeterApplication`] is captured
/// once at login. If we logged in while Voicemeeter was unreachable (e.g.
/// system just woke from sleep and the Voicemeeter process is still coming
/// back), `program` is latched to `None` and every `strip(i)`/`bus(i)` call
/// errors with "is not supported on `None`". Dropping the cached remote to
/// re-login is unsafe — the crate's global logout handle is one-shot, so the
/// last drop permanently disables `VoicemeeterRemote::new()`. Instead, when
/// we observe a `None` program we re-run `update_program()` against the live
/// DLL state; once Voicemeeter is reachable the next call here recovers.
///
/// [`VoicemeeterApplication`]: ::voicemeeter::types::VoicemeeterApplication
#[cfg(windows)]
pub(crate) fn vm_with<R>(
    holder: &std::sync::Arc<std::sync::Mutex<Option<::voicemeeter::VoicemeeterRemote>>>,
    f: impl FnOnce(&::voicemeeter::VoicemeeterRemote) -> R,
) -> Result<(R, bool)> {
    let mut guard = lock_sync(holder);
    let mut created = false;
    if guard.is_none() {
        let v = ::voicemeeter::VoicemeeterRemote::new().map_err(|e| anyhow!("{e}"))?;
        *guard = Some(v);
        created = true;
    }
    if let Some(vm) = guard.as_mut() {
        if vm.program == ::voicemeeter::types::VoicemeeterApplication::None {
            if let Err(e) = vm.update_program() {
                debug!("voicemeeter update_program: {e}");
            }
        }
    }
    let vm = guard
        .as_ref()
        .ok_or_else(|| anyhow!("voicemeeter init failed"))?
        .clone();
    drop(guard);
    let out = with_voicemeeter_io(|| {
        // Driving the dirty flag makes subsequent `get()` reads reflect live state.
        let _ = vm.is_parameters_dirty();
        f(&vm)
    });
    Ok((out, created))
}

/// Force-refresh the cached Voicemeeter [`VoicemeeterApplication`] without
/// rebuilding the remote. Intended for power-resume hooks (mirrors the OBS
/// client reset in `actions::obs::ensure`): when the OS wakes from sleep we
/// can't tell whether the Voicemeeter DLL still reports the same application
/// kind, and the cached value never auto-refreshes after init. This is a
/// no-op when nothing has been initialised yet — the next `vm_with` will do
/// a fresh login and discover the program normally.
///
/// Dropping + recreating the remote is intentionally avoided: the
/// [`voicemeeter`] crate auto-logs-out on the last `Drop`, after which
/// `VoicemeeterRemote::new()` returns `AlreadyLoggedOut` for the rest of
/// the process lifetime.
///
/// [`VoicemeeterApplication`]: ::voicemeeter::types::VoicemeeterApplication
#[cfg(windows)]
pub(crate) fn vm_refresh_program(
    holder: &std::sync::Arc<std::sync::Mutex<Option<::voicemeeter::VoicemeeterRemote>>>,
) {
    let mut guard = lock_sync(holder);
    let Some(vm) = guard.as_mut() else { return };
    if let Err(e) = with_voicemeeter_io(|| vm.update_program()) {
        debug!("voicemeeter refresh after resume: update_program failed: {e}");
    }
}

#[cfg(windows)]
fn poll_mutes(
    vm: ::voicemeeter::VoicemeeterRemote,
    params: Vec<(String, u32)>,
) -> HashMap<(String, u32), bool> {
    with_voicemeeter_io(|| {
        let _ = vm.is_parameters_dirty();
        let mut current = HashMap::new();
        for (target, index) in params {
            let Ok(t) = VmTarget::parse(&target, index) else { continue };
            if let Some(m) = t.mute_get(&vm) {
                current.insert((target, index), m);
            }
        }
        current
    })
}

/// Init if needed, then read gain (for meters). `bool` is `true` when this call created the client.
#[cfg(windows)]
pub(crate) fn vm_init_and_read_gain_db(
    holder: &std::sync::Arc<std::sync::Mutex<Option<::voicemeeter::VoicemeeterRemote>>>,
    target: &VmTarget,
) -> Result<(f32, bool)> {
    let (db_opt, created) = vm_with(holder, |vm| target.gain_get(vm))?;
    Ok((db_opt.unwrap_or(-60.0), created))
}

#[cfg(windows)]
pub(crate) fn try_spawn_vm_mute_poll_after_init(ctx: &ActionContext, created: bool) {
    if !created {
        return;
    }
    use std::sync::atomic::Ordering;
    if ctx
        .state
        .vm_mute_poll_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let holder = ctx.vm.clone();
    let inner = ctx.state.clone();
    tokio::spawn(async move {
        let handle = crate::state::AppHandle::from_inner(inner);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(400));
        loop {
            tick.tick().await;
            let params = handle.list_vm_mute_params().await;
            if params.is_empty() {
                continue;
            }
            let holder2 = holder.clone();
            let polled = match tokio::task::spawn_blocking(move || {
                let vm = lock_sync(&holder2).as_ref().cloned();
                let Some(vm) = vm else {
                    return HashMap::new();
                };
                poll_mutes(vm, params)
            })
            .await
            {
                Ok(m) => m,
                Err(_) => continue,
            };
            handle.merge_vm_mute_latch_from_poll(&polled).await;
            handle
                .sync_vm_mute(|t, i| polled.get(&(t.to_string(), i)).copied())
                .await;
        }
    });
}

#[cfg(windows)]
pub async fn gain(ctx: &ActionContext, spec: &ActionSpec, rotation: Option<i32>) -> Result<()> {
    let target = VmTarget::parse(str_field(spec, "target")?, u32_field(spec, "index", 0))?;
    let step = f32_field(spec, "step", 1.0);
    let ticks = rotation.unwrap_or(1) as f32;
    let delta = step * ticks;
    if delta.abs() < 1e-9 {
        return Ok(());
    }
    let op = if delta > 0.0 { "+=" } else { "-=" };
    let mag = format_script_float(delta.abs());
    let script = format!("{} {op} {mag};\r", target.gain_script_prefix());

    let holder = ctx.vm.clone();
    let (set_res, created) = tokio::task::spawn_blocking(move || {
        vm_with(&holder, |vm| vm.set_parameters(&script))
    })
    .await
    .map_err(|e| anyhow!("{e}"))??;
    set_res.map_err(|e| anyhow!("{e}"))?;
    try_spawn_vm_mute_poll_after_init(ctx, created);
    ctx.state.volume_meter_wake.notify_waiters();
    Ok(())
}

#[cfg(windows)]
pub async fn mute_toggle(ctx: &ActionContext, spec: &ActionSpec) -> Result<()> {
    let raw_target = str_field(spec, "target")?.to_string();
    let index = u32_field(spec, "index", 0);
    let target = VmTarget::parse(&raw_target, index)?;
    // Latch key preserves the raw config string (same casing as `sync_vm_mute`'s lookup).
    let key = (raw_target, index);
    let holder = ctx.vm.clone();

    // Always read the DLL mute flag here. `vm_mute_latch` can lag Voicemeeter UI / macros
    // (we merge the poll into the latch, but a press must not use a stale latch as baseline).
    let (prev_from_vm, created_read) = tokio::task::spawn_blocking({
        let h = holder.clone();
        let t = target.clone();
        move || vm_with(&h, |vm| t.mute_get(vm))
    })
    .await
    .map_err(|e| anyhow!("{e}"))??;
    try_spawn_vm_mute_poll_after_init(ctx, created_read);

    let prev_muted = match prev_from_vm {
        Some(b) => b,
        None => {
            let latch = ctx.state.vm_mute_latch.lock().await;
            latch.get(&key).copied().unwrap_or(false)
        }
    };

    let next_muted = !prev_muted;
    let snap = {
        let mut latch = ctx.state.vm_mute_latch.lock().await;
        latch.insert(key.clone(), next_muted);
        latch.clone()
    };
    // Redraw mute icons from latch now; otherwise they only update on the poll interval.
    ctx.handle()
        .sync_vm_mute(move |t, i| snap.get(&(t.to_string(), i)).copied())
        .await;

    let set_res = tokio::task::spawn_blocking({
        let h = holder.clone();
        let t = target.clone();
        move || vm_with(&h, |vm| t.mute_set(vm, next_muted)).map(|(r, _)| r)
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;

    if let Err(e) = set_res.and_then(|r| r) {
        let snap = {
            let mut latch = ctx.state.vm_mute_latch.lock().await;
            latch.insert(key, prev_muted);
            latch.clone()
        };
        ctx.handle()
            .sync_vm_mute(move |t, i| snap.get(&(t.to_string(), i)).copied())
            .await;
        return Err(e);
    }
    Ok(())
}

#[cfg(not(windows))]
pub async fn gain(_: &ActionContext, _: &ActionSpec, _: Option<i32>) -> Result<()> {
    Err(anyhow!("Voicemeeter is only supported on Windows"))
}

#[cfg(not(windows))]
pub async fn mute_toggle(_: &ActionContext, _: &ActionSpec) -> Result<()> {
    Err(anyhow!("Voicemeeter is only supported on Windows"))
}
