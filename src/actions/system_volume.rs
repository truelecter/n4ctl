//! System volume control for the default audio render endpoint.

use anyhow::Result;

use crate::{
    actions::{ActionContext, i32_field},
    config::ActionSpec,
};

#[cfg(windows)]
mod win {
    use anyhow::{Context, Result};
    use windows::Win32::{
        Media::Audio::{
            Endpoints::IAudioEndpointVolume, EDataFlow, ERole, IMMDeviceEnumerator,
            MMDeviceEnumerator,
        },
        System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance, CoInitializeEx,
                      COINIT_APARTMENTTHREADED},
    };

    /// Holds a default-render-endpoint volume controller.
    pub struct EndpointVolume;

    impl EndpointVolume {
        pub fn new() -> Self { Self }

        /// Adjust master volume by a percentage step (positive = louder, negative = quieter).
        /// Honors current mute state (it does not change it).
        pub fn step(&self, percent_step: i32) -> Result<()> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
                        .context("create MMDeviceEnumerator")?;
                let default = enumerator
                    .GetDefaultAudioEndpoint(EDataFlow(0), ERole(0))
                    .context("GetDefaultAudioEndpoint(render, console)")?;
                let endpoint: IAudioEndpointVolume = default
                    .Activate(CLSCTX_ALL, None)
                    .context("activate IAudioEndpointVolume")?;
                let current = endpoint
                    .GetMasterVolumeLevelScalar()
                    .context("get master volume")?;
                let next = (current + (percent_step as f32) / 100.0).clamp(0.0, 1.0);
                endpoint
                    .SetMasterVolumeLevelScalar(next, std::ptr::null())
                    .context("set master volume")?;
            }
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod win {
    use anyhow::{Result, anyhow};

    pub struct EndpointVolume;
    impl EndpointVolume {
        pub fn new() -> Self { Self }
        pub fn step(&self, _: i32) -> Result<()> {
            Err(anyhow!("system.volume is only implemented on Windows"))
        }
    }
}

pub struct VolumeBackend {
    inner: win::EndpointVolume,
}

impl VolumeBackend {
    pub fn new() -> Self {
        Self { inner: win::EndpointVolume::new() }
    }

    pub fn step(&self, pct: i32) -> Result<()> {
        self.inner.step(pct)
    }
}

pub async fn volume(ctx: &ActionContext, spec: &ActionSpec, rotation: Option<i32>) -> Result<()> {
    let base_step = i32_field(spec, "step", 2);
    let ticks = rotation.unwrap_or(1);
    let delta = base_step * ticks;
    ctx.volume.step(delta)
}
