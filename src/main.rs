mod actions;
mod cli;
mod config;
mod device;
mod inputs;
mod mapping;
mod power_session;
mod render;
mod service;
mod state;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use crate::cli::{Cli, Command};
use crate::state::AppState;

fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,n4ctl=debug".into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Install / uninstall / service dispatch are synchronous and must NOT
    // spin up a tokio runtime at the top level: `service_dispatcher::start`
    // blocks the calling thread and is expected to own the process.
    match cli.command.clone() {
        Some(Command::Install {
            mechanism,
            name,
            display_name,
            force,
            user,
            password,
            manual,
        }) => {
            return service::install(service::InstallOptions {
                mechanism,
                name,
                display_name,
                force,
                user,
                password,
                auto_start: !manual,
                config: cli.config,
            });
        }
        Some(Command::Uninstall { mechanism, name }) => {
            return service::uninstall(mechanism, &name);
        }
        Some(Command::Service { name }) => {
            return service::run_service(name, cli.config);
        }
        _ => {}
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    rt.block_on(async move {
        match cli.command.unwrap_or(Command::Run) {
            Command::List => list_devices().await,
            Command::Map => run_map_mode().await,
            Command::Raw => run_raw_mode().await,
            Command::Probe { max, dwell_ms } => run_probe_mode(max, dwell_ms).await,
            Command::Run => {
                let config_path = resolve_config_path(cli.config);
                run_main(config_path, CancellationToken::new()).await
            }
            // Handled synchronously above.
            Command::Install { .. } | Command::Uninstall { .. } | Command::Service { .. } => {
                unreachable!()
            }
        }
    })
}

/// Search order for the controller config, consulted only when the user did
/// not pass `--config`:
///   1. `$PWD/config.toml`
///   2. `~/n4ctl.toml`
///
/// Falls back to `$PWD/config.toml` (existence-unchecked) so the first
/// `config::load` call surfaces a clear "file not found" error.
pub fn resolve_config_path(cli_override: Option<PathBuf>) -> PathBuf {
    if let Some(p) = cli_override {
        return p;
    }
    let candidates = [
        std::env::current_dir().ok().map(|p| p.join("config.toml")),
        dirs::home_dir().map(|p| p.join("n4ctl.toml")),
    ];
    for c in candidates.iter().flatten() {
        if c.is_file() {
            return c.clone();
        }
    }
    std::env::current_dir()
        .ok()
        .map(|p| p.join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

/// Apply brightness and start from a blank state; shared by `run_session`,
/// `run_probe_mode`, and `run_map_mode`.
async fn prepare_device(dev: &mirajazz::device::Device, brightness: u8) {
    dev.set_brightness(brightness).await.ok();
    dev.clear_all_button_images().await.ok();
    dev.flush().await.ok();
}

async fn list_devices() -> Result<()> {
    let found = device::find_connected().await.context("enumerating devices")?;
    if found.is_empty() {
        println!("No matching Mirabox / Ajazz N4-family devices found.");
        return Ok(());
    }
    for d in &found {
        println!(
            "vid=0x{:04x} pid=0x{:04x} name={} serial={}",
            d.vendor_id,
            d.product_id,
            d.name.as_deref().unwrap_or("<no-name>"),
            d.serial_number.as_deref().unwrap_or("<none>")
        );
    }
    Ok(())
}

async fn run_probe_mode(max: u8, dwell_ms: u64) -> Result<()> {
    info!("Starting PROBE mode: will light image indices 0..{max} one at a time.");
    info!("Watch the device and write down which PHYSICAL position lights up for each idx.");

    let dev = device::open_first().await?;
    prepare_device(&dev, 70).await;

    let fmt = render::key_format();
    // Use a bright color so it's impossible to miss.
    let bright = render::solid_tile([40, 220, 120]);

    for idx in 0..max {
        info!("now painting image_idx={idx} -- which physical position lit up?");
        let _ = dev.set_button_image(idx, fmt, bright.clone()).await;
        let _ = dev.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(dwell_ms)).await;
        // Clear just this index (0xff would clear everything).
        let _ = dev.clear_button_image(idx).await;
        let _ = dev.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    info!("Probe complete. Re-run with --max N to probe more indices.");
    Ok(())
}

async fn run_raw_mode() -> Result<()> {
    info!("Starting RAW capture mode; press Ctrl-C to stop.");
    let dev = device::open_first().await?;
    let reader = dev.get_reader(inputs::process_input);
    loop {
        match reader
            .raw_read_data_with_timeout(64, std::time::Duration::from_millis(500))
            .await
        {
            Ok(Some(data)) => {
                let hex: Vec<String> = data.iter().map(|b| format!("{:02x}", b)).collect();
                info!("raw[{:02}]: {}", data.len(), hex.join(" "));
            }
            Ok(None) => {}
            Err(e) => {
                warn!("raw read error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn run_map_mode() -> Result<()> {
    info!("Starting MAP mode. Press each control one at a time to record its logical id.");
    info!("Indices: key_0..key_9 are the 10 displayed keys, strip_0..strip_4 the sensor strip, knob_0..knob_3 the encoders.");
    info!("Pressed key will flash green; released key will flash blue and then clear.");

    let dev = device::open_first().await?;
    prepare_device(&dev, 60).await;

    let green = render::solid_tile([32, 200, 64]);
    let blue = render::solid_tile([40, 80, 220]);
    let fmt = render::key_format();

    let reader = dev.get_reader(inputs::process_input);
    loop {
        match reader.read(Some(std::time::Duration::from_millis(200))).await {
            Ok(updates) => {
                for u in updates {
                    let slot = mapping::update_to_slot(&u);
                    info!("event={:?} slot={:?}", u, slot);
                    // Paint the *physical* position the user pressed, by
                    // routing SlotId -> image_index. Key and Strip slots both
                    // have displays on the N4; knobs do not.
                    //
                    // For strip zones we only flash on ButtonDown - the
                    // ButtonUp is the phantom "I changed state" event from
                    // mirajazz when another button is pressed, not a real
                    // finger lift.
                    let img = match (&u, slot) {
                        (mirajazz::state::DeviceStateUpdate::ButtonDown(_), _) => Some(green.clone()),
                        (mirajazz::state::DeviceStateUpdate::ButtonUp(_), Some(mapping::SlotId::Strip(_))) => None,
                        (mirajazz::state::DeviceStateUpdate::ButtonUp(_), _) => Some(blue.clone()),
                        _ => None,
                    };
                    if let (Some(s), Some(img)) = (slot, img) {
                        if let Some(idx) = s.image_index() {
                            let _ = dev.set_button_image(idx, fmt, img).await;
                            let _ = dev.flush().await;
                        }
                    }
                }
            }
            Err(e) => {
                warn!("map read error: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

pub async fn run_main(config_path: PathBuf, shutdown: CancellationToken) -> Result<()> {
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match run_session(&config_path, &shutdown).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!("session ended: {e:?}; reconnecting in 2s");
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                    _ = shutdown.cancelled() => return Ok(()),
                }
            }
        }
    }
}

/// One "session" = open device, render, run event loop. Returns on fatal
/// device/read error so the outer loop can reconnect, or cleanly when
/// `shutdown` is cancelled (service Stop, Ctrl-C wiring, …).
async fn run_session(config_path: &Path, shutdown: &CancellationToken) -> Result<()> {
    info!("Loading config from {}", config_path.display());
    let cfg = config::load(config_path).with_context(|| format!("load config {}", config_path.display()))?;

    let dev = match device::open_first().await {
        Ok(d) => d,
        Err(e) => {
            warn!("open device failed: {e}; retrying in 2s");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            return Err(e);
        }
    };
    let brightness = cfg.device.brightness.unwrap_or(60);
    prepare_device(&dev, brightness).await;

    let (evt_tx, evt_rx) = mpsc::unbounded_channel();

    let app = AppState::new(cfg, dev).await?;
    app.render_current_page().await?;

    // Separate channel to signal device failure from the input task.
    let (fail_tx, mut fail_rx) = tokio::sync::oneshot::channel::<String>();

    let reader = app.device().get_reader(inputs::process_input);
    let input_task = tokio::spawn(async move {
        let mut fail_tx = Some(fail_tx);
        let mut consecutive = 0u32;
        loop {
            match reader.read(Some(std::time::Duration::from_millis(200))).await {
                Ok(updates) => {
                    consecutive = 0;
                    let mut batch = Vec::new();
                    for u in updates {
                        batch.extend(mapping::expand_update(&u));
                    }
                    for event in mapping::coalesce_rotate_batch(batch) {
                        let _ = evt_tx.send(event);
                    }
                }
                Err(e) => {
                    consecutive = consecutive.saturating_add(1);
                    if consecutive <= 2 {
                        warn!("device read error: {e}");
                    }
                    if consecutive >= 10 {
                        if let Some(tx) = fail_tx.take() {
                            let _ = tx.send(format!("{e}"));
                        }
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });

    let watcher_task = tokio::spawn(config::watch(config_path.to_path_buf(), app.clone_handle()));

    let keepalive_task = {
        let d = app.device_arc();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tick.tick().await;
                if let Err(e) = d.keep_alive().await {
                    warn!("keepalive error: {e}");
                }
            }
        })
    };

    #[cfg(windows)]
    let (power_tx, mut power_rx) = mpsc::unbounded_channel::<power_session::PowerSessionEvent>();
    #[cfg(windows)]
    let power_watcher = power_session::spawn_watcher(power_tx);
    #[cfg(windows)]
    let power_listener = {
        use std::sync::atomic::{AtomicBool, Ordering};
        let handle = app.clone_handle();
        let dev_pw = app.device_arc();
        let asleep = std::sync::Arc::new(AtomicBool::new(false));
        // `asleep` collapses overlapping off/on events (e.g. suspend + lock
        // arriving back-to-back) so we don't double-sleep / double-restore.
        tokio::spawn(async move {
            while let Some(ev) = power_rx.recv().await {
                match ev {
                    power_session::PowerSessionEvent::DisplayOff => {
                        if !asleep.swap(true, Ordering::SeqCst) {
                            if let Err(e) = dev_pw.sleep().await {
                                warn!("device screen off (sleep/lock/shutdown): {e}");
                            } else {
                                info!("device screen off (sleep, shutdown, or session lock)");
                            }
                        }
                    }
                    power_session::PowerSessionEvent::DisplayOn => {
                        if asleep.swap(false, Ordering::SeqCst) {
                            let b = handle.configured_brightness();
                            dev_pw.set_brightness(b).await.ok();
                            dev_pw.keep_alive().await.ok();
                            match handle.render_current_page().await {
                                Ok(()) => info!("device display restored after resume or login"),
                                Err(e) => warn!("restore device display: {e}"),
                            }
                        }
                    }
                }
            }
        })
    };

    let dispatch = app.run_dispatch_loop(evt_rx);
    tokio::pin!(dispatch);

    let result: Result<()> = tokio::select! {
        _ = &mut dispatch => Ok(()),
        reason = &mut fail_rx => {
            let reason = reason.unwrap_or_else(|_| "input task ended".into());
            Err(anyhow::anyhow!("device failure: {reason}"))
        }
        _ = shutdown.cancelled() => Ok(()),
    };

    #[cfg(windows)]
    {
        power_watcher.request_shutdown();
        let _ = tokio::task::spawn_blocking(move || power_watcher.join()).await;
        power_listener.abort();
    }

    app.clone_handle().shutdown_gif_tasks().await;
    input_task.abort();
    watcher_task.abort();
    keepalive_task.abort();
    result
}
