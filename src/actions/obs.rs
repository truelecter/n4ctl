//! OBS WebSocket v5 actions.

use anyhow::{Context, Result, anyhow};
use futures_lite::StreamExt;
use obws::{Client, events::Event};
use tracing::{info, warn};

use crate::{
    actions::{ActionContext, str_field},
    config::ActionSpec,
    mapping::SlotId,
    state::AppHandle,
};

pub struct ObsClient {
    pub client: Client,
}

async fn ensure(ctx: &ActionContext) -> Result<()> {
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

    let client = Client::connect(host, port, password)
        .await
        .context("connect OBS")?;
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
