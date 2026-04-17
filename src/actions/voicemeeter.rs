//! Voicemeeter Remote API via `VoicemeeterRemote64.dll` (Windows).

use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::{
    actions::{ActionContext, f32_field, str_field, u32_field},
    config::ActionSpec,
};

#[cfg(windows)]
mod dll {
    use anyhow::{Context, Result, anyhow};
    use libloading::{Library, Symbol};
    use std::ffi::CString;
    use std::path::PathBuf;
    use std::sync::Mutex;

    type VbvmrLogin = unsafe extern "system" fn() -> i32;
    type VbvmrLogout = unsafe extern "system" fn() -> i32;
    type VbvmrSetParameterFloat =
        unsafe extern "system" fn(param: *const i8, value: f32) -> i32;
    type VbvmrGetParameterFloat =
        unsafe extern "system" fn(param: *const i8, out: *mut f32) -> i32;

    pub struct VmRemote {
        _lib: Library,
        logout: VbvmrLogout,
        set_f: VbvmrSetParameterFloat,
        get_f: VbvmrGetParameterFloat,
        lock: Mutex<()>,
    }

    unsafe impl Send for VmRemote {}
    unsafe impl Sync for VmRemote {}

    impl VmRemote {
        pub fn open() -> Result<Self> {
            let path = find_dll().context("find VoicemeeterRemote64.dll")?;
            let lib = unsafe { Library::new(&path) }
                .with_context(|| format!("load {}", path.display()))?;
            let (logout, set_f, get_f, rc) = unsafe {
                let login: Symbol<VbvmrLogin> = lib.get(b"VBVMR_Login")?;
                let logout: Symbol<VbvmrLogout> = lib.get(b"VBVMR_Logout")?;
                let set_f: Symbol<VbvmrSetParameterFloat> =
                    lib.get(b"VBVMR_SetParameterFloat")?;
                let get_f: Symbol<VbvmrGetParameterFloat> =
                    lib.get(b"VBVMR_GetParameterFloat")?;
                let rc = login();
                (*logout, *set_f, *get_f, rc)
            };
            if rc < 0 {
                return Err(anyhow!("VBVMR_Login returned {rc}"));
            }
            Ok(Self {
                _lib: lib,
                logout,
                set_f,
                get_f,
                lock: Mutex::new(()),
            })
        }

        pub fn set_parameter_float(&self, param: &str, value: f32) -> Result<()> {
            let _g = self.lock.lock().unwrap();
            let c = CString::new(param).context("CString param")?;
            let rc = unsafe { (self.set_f)(c.as_ptr(), value) };
            if rc < 0 {
                return Err(anyhow!("VBVMR_SetParameterFloat({param})={rc}"));
            }
            Ok(())
        }

        pub fn get_parameter_float(&self, param: &str) -> Result<f32> {
            let _g = self.lock.lock().unwrap();
            let c = CString::new(param).context("CString param")?;
            let mut out = 0.0_f32;
            let rc = unsafe { (self.get_f)(c.as_ptr(), &mut out as *mut f32) };
            if rc < 0 {
                return Err(anyhow!("VBVMR_GetParameterFloat({param})={rc}"));
            }
            Ok(out)
        }
    }

    impl Drop for VmRemote {
        fn drop(&mut self) {
            unsafe {
                let _ = (self.logout)();
            }
        }
    }

    pub fn find_dll() -> Result<PathBuf> {
        let candidates = [
            r"C:\Program Files (x86)\VB\Voicemeeter\VoicemeeterRemote64.dll",
            r"C:\Program Files\VB\Voicemeeter\VoicemeeterRemote64.dll",
        ];
        for c in candidates {
            let p = PathBuf::from(c);
            if p.exists() {
                return Ok(p);
            }
        }
        Err(anyhow!("VoicemeeterRemote64.dll not found in standard locations"))
    }
}

#[cfg(not(windows))]
mod dll {
    use anyhow::{Result, anyhow};
    pub struct VmRemote;
    impl VmRemote {
        pub fn open() -> Result<Self> { Err(anyhow!("Voicemeeter is Windows-only")) }
        pub fn set_parameter_float(&self, _: &str, _: f32) -> Result<()> { Err(anyhow!("n/a")) }
        pub fn get_parameter_float(&self, _: &str) -> Result<f32> { Err(anyhow!("n/a")) }
    }
}

/// Shared Voicemeeter session. The last Arc holder's `Drop` calls Logout.
pub type VoicemeeterClient = Arc<dll::VmRemote>;

async fn ensure(ctx: &ActionContext) -> Result<VoicemeeterClient> {
    let mut guard = ctx.vm.lock().await;
    if let Some(vm) = guard.as_ref() {
        return Ok(vm.clone());
    }
    let vm: VoicemeeterClient = Arc::new(dll::VmRemote::open()?);
    *guard = Some(vm.clone());
    drop(guard);

    // Spawn mute-state polling task using the same session.
    let inner = ctx.state.clone();
    let vm_for_poll = vm.clone();
    tokio::spawn(async move {
        let handle = crate::state::AppHandle::from_inner(inner);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(400));
        loop {
            tick.tick().await;
            let params = handle.list_vm_mute_params().await;
            if params.is_empty() {
                continue;
            }
            let mut current = std::collections::HashMap::new();
            for (target, index) in params {
                if let Ok(param) = target_param(&target, index, "Mute") {
                    if let Ok(v) = vm_for_poll.get_parameter_float(&param) {
                        current.insert((target, index), v > 0.5);
                    }
                }
            }
            handle
                .sync_vm_mute(|t, i| current.get(&(t.to_string(), i)).copied())
                .await;
        }
    });

    Ok(vm)
}

fn target_param(target: &str, index: u32, field: &str) -> Result<String> {
    let t = target.to_ascii_lowercase();
    let prefix = match t.as_str() {
        "strip" => "Strip",
        "bus" => "Bus",
        other => return Err(anyhow!("voicemeeter target must be Strip or Bus, got '{other}'")),
    };
    Ok(format!("{prefix}[{index}].{field}"))
}

pub async fn gain(ctx: &ActionContext, spec: &ActionSpec, rotation: Option<i32>) -> Result<()> {
    let vm = ensure(ctx).await?;
    let target = str_field(spec, "target")?.to_string();
    let index = u32_field(spec, "index", 0);
    let step = f32_field(spec, "step", 1.0);
    let ticks = rotation.unwrap_or(1) as f32;
    let param = target_param(&target, index, "Gain")?;
    let current = vm.get_parameter_float(&param).unwrap_or(0.0);
    let next = (current + step * ticks).clamp(-60.0, 12.0);
    vm.set_parameter_float(&param, next)?;
    Ok(())
}

pub async fn mute_toggle(ctx: &ActionContext, spec: &ActionSpec) -> Result<()> {
    let vm = ensure(ctx).await?;
    let target = str_field(spec, "target")?.to_string();
    let index = u32_field(spec, "index", 0);
    let param = target_param(&target, index, "Mute")?;
    let cur = vm.get_parameter_float(&param).unwrap_or(0.0);
    let next = if cur > 0.5 { 0.0 } else { 1.0 };
    vm.set_parameter_float(&param, next)?;
    Ok(())
}
