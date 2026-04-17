use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "n4ctl",
    version,
    about = "Standalone controller for Mirabox N4 (keys, sensor strip, knobs)",
    propagate_version = true
)]
pub struct Cli {
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Command {
    /// Run the controller with the given config (default).
    Run,
    /// List connected Mirabox / Ajazz N4-family devices.
    List,
    /// Diagnostic: log logical slot events as you press each control.
    Map,
    /// Diagnostic: log raw HID packets coming from the device.
    Raw,
    /// Diagnostic: light each display index one at a time so you can see
    /// which physical position it corresponds to on your device.
    Probe {
        /// Max image index to try (exclusive). Default 16.
        #[arg(long, default_value_t = 16)]
        max: u8,
        /// Dwell time per index, in milliseconds.
        #[arg(long, default_value_t = 1200)]
        dwell_ms: u64,
    },
}
