use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Default autostart entry name (scheduled task name / SCM service key).
pub const DEFAULT_SERVICE_NAME: &str = "n4ctl";

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

/// Mechanism used to autostart the app on Windows. Scheduled-task is the
/// default because it runs in the user's session, which is required for
/// Voicemeeter Remote and OBS WebSocket to reach their GUI counterparts
/// (session-0 services are isolated from user-session IPC).
#[derive(Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq)]
pub enum InstallMechanism {
    /// Scheduled Task with a "At log on" trigger, running as the user.
    /// **Works with Voicemeeter and OBS.** Default.
    #[default]
    Task,
    /// Windows Service in SCM (session 0). Boots before login, but cannot
    /// talk to user-session IPC (Voicemeeter / OBS will fail silently).
    Service,
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
    /// Register an autostart entry so n4ctl runs on boot / login.
    ///
    /// The global `--config <path>` is resolved to an absolute path and
    /// baked into the autostart entry, so it doesn't matter what the task's
    /// or service's working directory is when it launches.
    Install {
        /// How to autostart: `task` (user-session Scheduled Task, default)
        /// or `service` (session-0 Windows Service — Voicemeeter/OBS won't
        /// work under this mode).
        #[arg(long, value_enum, default_value_t = InstallMechanism::default())]
        mechanism: InstallMechanism,
        /// Entry name (task name in Task Scheduler, or service key in SCM).
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
        /// Display name / description. Defaults to `--name`.
        #[arg(long)]
        display_name: Option<String>,
        /// Replace an existing entry with the same name.
        #[arg(long)]
        force: bool,
        /// Account to run under:
        ///   * `task`: defaults to the current user (`$USERDOMAIN\\$USERNAME`).
        ///   * `service`: defaults to `LocalSystem`.
        ///
        /// Override to run as a specific user (e.g. `.\\Alice`).
        #[arg(long)]
        user: Option<String>,
        /// Password for `--user`. Required for service installs running as
        /// a non-virtual account; optional for task installs (tasks launched
        /// at logon use the user's interactive token).
        #[arg(long)]
        password: Option<String>,
        /// Install the entry but do not auto-start it.
        ///
        /// * `task`: creates the task with its logon trigger disabled; you
        ///   can run it manually via `schtasks /Run /TN <name>`.
        /// * `service`: installs with `Start = DemandStart`.
        #[arg(long)]
        manual: bool,
    },
    /// Remove a previously-installed autostart entry.
    Uninstall {
        /// Which autostart mechanism to look for.
        #[arg(long, value_enum, default_value_t = InstallMechanism::default())]
        mechanism: InstallMechanism,
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
    /// Internal: invoked by the Service Control Manager when the installed
    /// service starts. Not meant to be called directly.
    #[command(hide = true)]
    Service {
        #[arg(long, default_value = DEFAULT_SERVICE_NAME)]
        name: String,
    },
}
