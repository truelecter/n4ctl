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
    let _guard = voicemeeter_io_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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

/// `Strip[i].Gain` / `Bus[i].Gain` prefix for [`VBVMR_SetParameters`] relative scripts.
#[cfg(windows)]
fn target_gain_script_prefix(target: &str, index: u32) -> Result<String> {
    let t = target.to_ascii_lowercase();
    match t.as_str() {
        "strip" => Ok(format!("Strip[{index}].Gain")),
        "bus" => Ok(format!("Bus[{index}].Gain")),
        other => Err(anyhow!("voicemeeter target must be Strip or Bus, got '{other}'")),
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
            let muted = if target.eq_ignore_ascii_case("strip") {
                vm.parameters()
                    .strip(index as usize)
                    .ok()
                    .and_then(|s| s.mute().get().ok())
            } else if target.eq_ignore_ascii_case("bus") {
                vm.parameters()
                    .bus(index as usize)
                    .ok()
                    .and_then(|b| b.mute().get().ok())
            } else {
                None
            };
            if let Some(m) = muted {
                current.insert((target, index), m);
            }
        }
        current
    })
}

#[cfg(windows)]
fn read_mute_state(
    vm: &::voicemeeter::VoicemeeterRemote,
    target: &str,
    index: u32,
) -> Option<bool> {
    with_voicemeeter_io(|| {
        let _ = vm.is_parameters_dirty();
        if target.eq_ignore_ascii_case("strip") {
            vm.parameters()
                .strip(index as usize)
                .ok()?
                .mute()
                .get()
                .ok()
        } else if target.eq_ignore_ascii_case("bus") {
            vm.parameters()
                .bus(index as usize)
                .ok()?
                .mute()
                .get()
                .ok()
        } else {
            None
        }
    })
}

/// Current `Gain` in dB for UI meters (`-60..12` typical).
#[cfg(windows)]
pub fn read_gain_db(vm: &::voicemeeter::VoicemeeterRemote, target: &str, index: u32) -> Option<f32> {
    with_voicemeeter_io(|| {
        let _ = vm.is_parameters_dirty();
        if target.eq_ignore_ascii_case("strip") {
            vm.parameters()
                .strip(index as usize)
                .ok()?
                .gain()
                .get()
                .ok()
        } else if target.eq_ignore_ascii_case("bus") {
            vm.parameters()
                .bus(index as usize)
                .ok()?
                .gain()
                .get()
                .ok()
        } else {
            None
        }
    })
}

#[cfg(windows)]
fn set_mute_state(
    vm: &::voicemeeter::VoicemeeterRemote,
    target: &str,
    index: u32,
    muted: bool,
) -> Result<()> {
    with_voicemeeter_io(|| {
        let _ = vm.is_parameters_dirty();
        if target.eq_ignore_ascii_case("strip") {
            vm.parameters()
                .strip(index as usize)
                .map_err(|e| anyhow!("{e}"))?
                .mute()
                .set(muted)
                .map_err(|e| anyhow!("{e}"))
        } else if target.eq_ignore_ascii_case("bus") {
            vm.parameters()
                .bus(index as usize)
                .map_err(|e| anyhow!("{e}"))?
                .mute()
                .set(muted)
                .map_err(|e| anyhow!("{e}"))
        } else {
            Err(anyhow!("voicemeeter target must be Strip or Bus"))
        }
    })
}

/// Returns `Ok(true)` if this call created the Remote (first init).
#[cfg(windows)]
pub(crate) fn vm_ensure_initialized(
    holder: &std::sync::Arc<std::sync::Mutex<Option<::voicemeeter::VoicemeeterRemote>>>,
) -> Result<bool> {
    let mut guard = holder.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return Ok(false);
    }
    let v = ::voicemeeter::VoicemeeterRemote::new().map_err(|e| anyhow!("{e}"))?;
    *guard = Some(v);
    Ok(true)
}

/// Init if needed, then read gain (for meters). `bool` is `true` when this call created the client.
#[cfg(windows)]
pub(crate) fn vm_init_and_read_gain_db(
    holder: &std::sync::Arc<std::sync::Mutex<Option<::voicemeeter::VoicemeeterRemote>>>,
    target: &str,
    index: u32,
) -> Result<(f32, bool)> {
    let mut guard = holder.lock().unwrap_or_else(|e| e.into_inner());
    let mut created = false;
    if guard.is_none() {
        let v = ::voicemeeter::VoicemeeterRemote::new().map_err(|e| anyhow!("{e}"))?;
        *guard = Some(v);
        created = true;
    }
    let vm = guard
        .as_ref()
        .ok_or_else(|| anyhow!("voicemeeter init failed"))?
        .clone();
    drop(guard);
    let db = read_gain_db(&vm, target, index).unwrap_or(-60.0);
    Ok((db, created))
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
                let vm = {
                    let guard = holder2.lock().unwrap_or_else(|e| e.into_inner());
                    guard.as_ref().cloned()
                };
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
    let target = str_field(spec, "target")?;
    let index = u32_field(spec, "index", 0);
    let step = f32_field(spec, "step", 1.0);
    let ticks = rotation.unwrap_or(1) as f32;
    let delta = step * ticks;
    if delta.abs() < 1e-9 {
        return Ok(());
    }
    let prefix = target_gain_script_prefix(target, index)?;
    let op = if delta > 0.0 { "+=" } else { "-=" };
    let mag = format_script_float(delta.abs());
    let script = format!("{prefix} {op} {mag};\r");

    let holder = ctx.vm.clone();
    let created = tokio::task::spawn_blocking(move || {
        let mut guard = holder.lock().unwrap_or_else(|e| e.into_inner());
        let mut created = false;
        if guard.is_none() {
            let v = ::voicemeeter::VoicemeeterRemote::new().map_err(|e| anyhow!("{e}"))?;
            *guard = Some(v);
            created = true;
        }
        let vm = guard
            .as_ref()
            .ok_or_else(|| anyhow!("voicemeeter missing after init"))?
            .clone();
        drop(guard);
        with_voicemeeter_io(|| vm.set_parameters(&script)).map_err(|e| anyhow!("{e}"))?;
        Ok::<bool, anyhow::Error>(created)
    })
    .await
    .map_err(|e| anyhow!("{e}"))??;
    try_spawn_vm_mute_poll_after_init(ctx, created);
    ctx.state.volume_meter_wake.notify_waiters();
    Ok(())
}

#[cfg(windows)]
pub async fn mute_toggle(ctx: &ActionContext, spec: &ActionSpec) -> Result<()> {
    let target = str_field(spec, "target")?.to_string();
    let index = u32_field(spec, "index", 0);
    let key = (target.clone(), index);
    let holder = ctx.vm.clone();

    let created_init = tokio::task::spawn_blocking({
        let h = holder.clone();
        move || vm_ensure_initialized(&h)
    })
    .await
    .map_err(|e| anyhow!("{e}"))??;
    try_spawn_vm_mute_poll_after_init(ctx, created_init);

    // Always read the DLL mute flag here. `vm_mute_latch` can lag Voicemeeter UI / macros
    // (we merge the poll into the latch, but a press must not use a stale latch as baseline).
    let prev_from_vm = tokio::task::spawn_blocking({
        let h = holder.clone();
        let t = target.clone();
        move || {
            let vm = {
                let guard = h.lock().unwrap_or_else(|e| e.into_inner());
                guard.as_ref().cloned()
            };
            let Some(vm) = vm else {
                return None;
            };
            read_mute_state(&vm, &t, index)
        }
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;

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

    let h = holder.clone();
    let t = target.clone();
    let set_res = tokio::task::spawn_blocking(move || {
        let vm = {
            let guard = h.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        let Some(vm) = vm else {
            return Err(anyhow!("voicemeeter not initialized"));
        };
        set_mute_state(&vm, &t, index, next_muted)
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;

    if let Err(e) = set_res {
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
