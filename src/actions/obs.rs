//! OBS WebSocket v5 actions.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use futures_lite::StreamExt;
use obws::{Client, events::Event};
use obws::requests::inputs::{InputId, Volume};
use tracing::{info, warn};

use crate::{
    actions::{ActionContext, f32_field, str_field},
    config::ActionSpec,
    mapping::SlotId,
    state::AppHandle,
};

/// How long after a failed connect attempt we skip further connects so that
/// rapid button presses while OBS is offline don't queue up 2s timeouts.
const OBS_CONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Hard cap on a single `Client::connect` call so it can't freeze a dispatch
/// task for too long even if DNS / TCP stalls.
const OBS_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct ObsClient {
    pub client: Client,
}

async fn ensure(ctx: &ActionContext) -> Result<()> {
    // Fast path: already connected.
    if ctx.obs.lock().await.is_some() {
        return Ok(());
    }
    // Cooldown: bail early if a recent connect failed.
    if let Some(t) = *ctx.obs_backoff.lock().await {
        if t.elapsed() < OBS_CONNECT_BACKOFF {
            return Err(anyhow!("OBS connect in backoff"));
        }
    }

    let mut guard = ctx.obs.lock().await;
    if guard.is_some() {
        return Ok(());
    }
    let cfg = ctx.state.config.load_full();
    let Some(o) = cfg.obs.clone() else {
        return Err(anyhow!("OBS action used but [obs] section missing in config"));
    };

    let (host, port) = parse_ws(&o.url)?;
    let password = o
        .password
        .clone()
        .or_else(|| o.password_env.as_ref().and_then(|k| std::env::var(k).ok()));

    let connect = Client::connect(host, port, password);
    let client = match tokio::time::timeout(OBS_CONNECT_TIMEOUT, connect).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            *ctx.obs_backoff.lock().await = Some(Instant::now());
            return Err(anyhow::Error::new(e).context("connect OBS"));
        }
        Err(_) => {
            *ctx.obs_backoff.lock().await = Some(Instant::now());
            return Err(anyhow!("OBS connect timed out after {OBS_CONNECT_TIMEOUT:?}"));
        }
    };
    *ctx.obs_backoff.lock().await = None;
    info!("Connected to OBS WebSocket");

    // Prime scene + virtual cam state
    if let Ok(current) = client.scenes().current_program_scene().await {
        AppHandle::from_inner(ctx.state.clone())
            .sync_obs_scene(&current.id.name)
            .await;
    }
    if let Ok(status) = client.virtual_cam().status().await {
        AppHandle::from_inner(ctx.state.clone())
            .sync_virtual_cam(status)
            .await;
    }

    // Subscribe to events; updates 2-state icons for scene/virtual cam in real time.
    match client.events() {
        Ok(stream) => {
            let inner = ctx.state.clone();
            tokio::spawn(async move {
                let handle = AppHandle::from_inner(inner);
                tokio::pin!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        Event::CurrentProgramSceneChanged { id } => {
                            handle.sync_obs_scene(&id.name).await;
                        }
                        Event::VirtualcamStateChanged { active, .. } => {
                            handle.sync_virtual_cam(active).await;
                        }
                        _ => {}
                    }
                }
                warn!("OBS event stream closed");
            });
        }
        Err(e) => warn!("could not subscribe to OBS events: {e}"),
    }

    *guard = Some(ObsClient { client });
    Ok(())
}

fn parse_ws(url: &str) -> Result<(String, u16)> {
    let s = url.trim_start_matches("ws://").trim_start_matches("wss://");
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().context("parse port")?),
        None => (s.to_string(), 4455),
    };
    Ok((host, port))
}

pub async fn scene(ctx: &ActionContext, spec: &ActionSpec) -> Result<()> {
    ensure(ctx).await?;
    let scene = str_field(spec, "scene")?.to_string();
    let collection = spec
        .get("collection")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let guard = ctx.obs.lock().await;
    let client = &guard.as_ref().context("obs client gone")?.client;

    if let Some(collection) = collection.as_deref() {
        let current = client
            .scene_collections()
            .current()
            .await
            .context("get current scene collection")?;
        if current != collection {
            client
                .scene_collections()
                .set_current(collection)
                .await
                .context("set scene collection")?;
        }
    }
    client
        .scenes()
        .set_current_program_scene(scene.as_str())
        .await
        .with_context(|| format!("SetCurrentProgramScene({scene})"))?;
    Ok(())
}

/// Adjust an OBS input's mixer volume **in dB** relative to its current level
/// (`GetInputVolume`, apply `step_db` × encoder ticks, `SetInputVolume` as dB).
pub async fn input_volume(ctx: &ActionContext, spec: &ActionSpec, rotation: Option<i32>) -> Result<()> {
    ensure(ctx).await?;
    let input = str_field(spec, "input")?;
    let step_db = f32_field(spec, "step_db", 1.0);
    let ticks = rotation.unwrap_or(1) as f32;

    let guard = ctx.obs.lock().await;
    let client = &guard.as_ref().context("obs client gone")?.client;
    let id = InputId::Name(input);
    let vol = client.inputs().volume(id).await.context("GetInputVolume")?;
    // OBS mixer is effectively bounded; clamp avoids rejected RPC on edge values.
    let next_db = (vol.db + step_db * ticks).clamp(-100.0, 30.0);
    client
        .inputs()
        .set_volume(id, Volume::Db(next_db))
        .await
        .context("SetInputVolume")?;
    Ok(())
}

pub async fn virtual_cam(ctx: &ActionContext, slot: SlotId, _spec: &ActionSpec) -> Result<()> {
    ensure(ctx).await?;
    let guard = ctx.obs.lock().await;
    let client = &guard.as_ref().context("obs client gone")?.client;
    let active = client
        .virtual_cam()
        .toggle()
        .await
        .context("toggle virtual cam")?;
    drop(guard);
    ctx.handle()
        .set_slot_state(slot, if active { 1 } else { 0 })
        .await;
    Ok(())
}
