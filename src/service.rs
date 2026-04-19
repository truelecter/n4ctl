//! Windows autostart support — install / uninstall / SCM dispatch.
//!
//! Two mechanisms are offered at install time:
//!
//! * **Scheduled Task** (default): a user-session task with a "At log on"
//!   trigger. This is the right choice for an interactive app because the
//!   Voicemeeter Remote DLL and OBS WebSocket both talk to GUI counterparts
//!   that live in the logged-on user's session; a session-0 service cannot
//!   reach them.
//! * **Windows Service**: registered with SCM, runs in session 0 as
//!   LocalSystem (or a configured account). Starts before login, but
//!   Voicemeeter / OBS integration will fail silently. Kept as an option
//!   for kiosk / always-on scenarios where only the device matters.
//!
//! Non-Windows builds get runtime-error stubs so the CLI surface stays
//! uniform.

use std::path::PathBuf;

#[cfg(not(windows))]
use anyhow::{Result, anyhow};

use crate::cli::InstallMechanism;

pub struct InstallOptions {
    pub mechanism: InstallMechanism,
    pub name: String,
    pub display_name: Option<String>,
    pub force: bool,
    pub user: Option<String>,
    pub password: Option<String>,
    pub auto_start: bool,
    pub config: Option<PathBuf>,
}

#[cfg(not(windows))]
pub fn install(_opts: InstallOptions) -> Result<()> {
    Err(anyhow!(
        "`install` is only supported on Windows; use your init system (systemd, launchd, …) directly"
    ))
}

#[cfg(not(windows))]
pub fn uninstall(_mechanism: InstallMechanism, _name: &str) -> Result<()> {
    Err(anyhow!("`uninstall` is only supported on Windows"))
}

#[cfg(not(windows))]
pub fn run_service(_name: String, _config: Option<PathBuf>) -> Result<()> {
    Err(anyhow!("`service` is only supported on Windows"))
}

#[cfg(windows)]
pub use windows_imp::{install, run_service, uninstall};

#[cfg(windows)]
mod windows_imp {
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
        sync::OnceLock,
        time::Duration,
    };

    use anyhow::{Context, Result, anyhow, bail};
    use tokio_util::sync::CancellationToken;
    use tracing::{info, warn};
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
            ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    use super::InstallOptions;
    use crate::cli::InstallMechanism;

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(30);

    /// `ERROR_SERVICE_DOES_NOT_EXIST` — returned by `open_service` when the
    /// requested service isn't registered.
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

    // ------------------------------------------------------------------
    // Public entry points (dispatch on InstallMechanism)
    // ------------------------------------------------------------------

    pub fn install(opts: InstallOptions) -> Result<()> {
        match opts.mechanism {
            InstallMechanism::Service => install_service(opts),
            InstallMechanism::Task => install_task(opts),
        }
    }

    pub fn uninstall(mechanism: InstallMechanism, name: &str) -> Result<()> {
        match mechanism {
            InstallMechanism::Service => uninstall_service(name),
            InstallMechanism::Task => uninstall_task(name),
        }
    }

    // ------------------------------------------------------------------
    // Scheduled Task (user-session autostart)
    // ------------------------------------------------------------------

    fn install_task(opts: InstallOptions) -> Result<()> {
        let exe_path = canonical_no_verbatim(&std::env::current_exe()?);
        let working_dir = exe_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();

        // Point the task at the GUI-subsystem launcher sibling rather than
        // at `n4ctl.exe` directly. The launcher (`src/bin/launcher.rs`)
        // spawns `n4ctl.exe` with `CREATE_NO_WINDOW`, so at logon there's
        // no console allocation and no flash window. See its module docs
        // for the design rationale.
        let launcher_path = working_dir.join("n4ctl-launcher.exe");
        if !launcher_path.is_file() {
            bail!(
                "expected launcher sibling at {} but it is missing. \
                 Build with `cargo build --release --bin n4ctl-launcher` \
                 and deploy it next to n4ctl.exe before running `install --mechanism task`.",
                launcher_path.display(),
            );
        }

        let config_abs = opts
            .config
            .as_deref()
            .map(absolute_config_path)
            .transpose()?;

        let user = opts
            .user
            .clone()
            .map(Result::Ok)
            .unwrap_or_else(current_user_domain_account)?;

        let description = opts
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{} — Mirabox N4 controller", opts.name));

        let args = match config_abs.as_ref() {
            Some(cfg) => format!("--config \"{}\"", cfg.display()),
            None => String::new(),
        };

        let xml = build_task_xml(TaskXmlArgs {
            description: &description,
            user: &user,
            exe: &launcher_path.display().to_string(),
            args: &args,
            working_dir: &working_dir.display().to_string(),
            trigger_enabled: opts.auto_start,
        });

        // Task Scheduler expects UTF-16 with a BOM — UTF-8 works on modern
        // Windows but UTF-16 is the documented contract, so we play safe.
        let tmp = std::env::temp_dir().join(format!("n4ctl-task-{}.xml", opts.name));
        write_utf16_bom(&tmp, &xml).context("writing temp task XML")?;

        let mut cmd = std::process::Command::new("schtasks");
        cmd.arg("/Create")
            .arg("/TN")
            .arg(&opts.name)
            .arg("/XML")
            .arg(&tmp)
            .arg("/RU")
            .arg(&user);
        if opts.force {
            cmd.arg("/F");
        }
        if let Some(pw) = opts.password.as_ref() {
            cmd.arg("/RP").arg(pw);
        }

        let output = cmd.output().context("invoking schtasks.exe")?;
        let _ = std::fs::remove_file(&tmp);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!(
                "schtasks /Create failed (exit {}). stdout: {}  stderr: {}{}",
                output.status.code().unwrap_or(-1),
                stdout.trim(),
                stderr.trim(),
                if !opts.force {
                    "  (hint: pass --force to overwrite an existing task)"
                } else {
                    ""
                },
            );
        }

        info!(
            "Installed scheduled task '{}' as user '{}' (at-logon trigger: {}), launcher: {}, config: {}",
            opts.name,
            user,
            if opts.auto_start { "enabled" } else { "disabled" },
            launcher_path.display(),
            config_abs
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none — uses search path>".into()),
        );
        info!(
            "Run now with: `schtasks /Run /TN {}`  (or reboot / log back in)",
            opts.name
        );
        Ok(())
    }

    fn uninstall_task(name: &str) -> Result<()> {
        let output = std::process::Command::new("schtasks")
            .arg("/Delete")
            .arg("/TN")
            .arg(name)
            .arg("/F")
            .output()
            .context("invoking schtasks.exe")?;

        if output.status.success() {
            info!("Uninstalled scheduled task '{}'", name);
            return Ok(());
        }

        // schtasks prints localized "ERROR: The system cannot find the file
        // specified." when the task doesn't exist; exit code is usually 1.
        // Treat "not found" as a no-op so uninstall is idempotent.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{} {}", stderr, String::from_utf8_lossy(&output.stdout));
        if combined.contains("cannot find") || combined.contains("does not exist") {
            info!("Scheduled task '{}' already absent", name);
            return Ok(());
        }

        bail!(
            "schtasks /Delete failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            combined.trim(),
        );
    }

    /// Resolve the current user as `DOMAIN\\User`, falling back to
    /// `.\\User` (local machine) when `USERDOMAIN` isn't set.
    fn current_user_domain_account() -> Result<String> {
        let username = std::env::var("USERNAME")
            .map_err(|_| anyhow!("USERNAME env var not set; pass --user explicitly"))?;
        let domain = std::env::var("USERDOMAIN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".to_string());
        Ok(format!("{domain}\\{username}"))
    }

    struct TaskXmlArgs<'a> {
        description: &'a str,
        user: &'a str,
        exe: &'a str,
        args: &'a str,
        working_dir: &'a str,
        trigger_enabled: bool,
    }

    fn build_task_xml(a: TaskXmlArgs<'_>) -> String {
        // NOTE: on laptops Task Scheduler refuses to start tasks by default
        // if "DisallowStartIfOnBatteries" is true (the default for tasks
        // created via the UI). We explicitly set it to false so the device
        // controller works whether you're plugged in or not.
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
    <Author>n4ctl</Author>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>{trigger_enabled}</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{args}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
            description = xml_escape(a.description),
            user = xml_escape(a.user),
            exe = xml_escape(a.exe),
            args = xml_escape(a.args),
            working_dir = xml_escape(a.working_dir),
            trigger_enabled = if a.trigger_enabled { "true" } else { "false" },
        )
    }

    fn xml_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                other => out.push(other),
            }
        }
        out
    }

    /// Write `s` as little-endian UTF-16 with a BOM — the format the Task
    /// Scheduler XML schema is documented to accept.
    fn write_utf16_bom(path: &Path, s: &str) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        f.write_all(&[0xFF, 0xFE])?;
        for u in s.encode_utf16() {
            f.write_all(&u.to_le_bytes())?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Windows Service (session-0 autostart)
    // ------------------------------------------------------------------

    fn install_service(opts: InstallOptions) -> Result<()> {
        let exe_path = canonical_no_verbatim(&std::env::current_exe()?);
        let config_abs = opts
            .config
            .as_deref()
            .map(absolute_config_path)
            .transpose()?;

        let display_name = opts.display_name.clone().unwrap_or_else(|| opts.name.clone());

        let mut launch_args: Vec<OsString> = Vec::new();
        if let Some(cfg) = config_abs.as_ref() {
            launch_args.push("--config".into());
            launch_args.push(cfg.into());
        }
        launch_args.push("service".into());
        launch_args.push("--name".into());
        launch_args.push(OsString::from(&opts.name));

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .context("opening Service Control Manager (admin required)")?;

        if opts.force {
            if let Err(e) = try_stop_and_delete(&opts.name) {
                warn!("could not fully remove existing service '{}': {e:#}", opts.name);
            }
        }

        let service_info = ServiceInfo {
            name: OsString::from(&opts.name),
            display_name: OsString::from(&display_name),
            service_type: SERVICE_TYPE,
            start_type: if opts.auto_start {
                ServiceStartType::AutoStart
            } else {
                ServiceStartType::OnDemand
            },
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path,
            launch_arguments: launch_args,
            dependencies: vec![],
            account_name: opts.user.as_deref().map(OsString::from),
            account_password: opts.password.as_deref().map(OsString::from),
        };

        let service = manager
            .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
            .with_context(|| {
                format!(
                    "creating service '{}'. Pass --force to replace an existing service.",
                    opts.name
                )
            })?;

        let _ = service.set_description("Standalone controller for Mirabox N4 devices");

        warn!(
            "Service installed in session 0: Voicemeeter Remote and OBS WebSocket \
             will not be reachable from this mode. For those, reinstall with \
             `--mechanism task` (the default)."
        );
        info!(
            "Installed service '{}' (display: '{}') with config {:?}; start type: {}",
            opts.name,
            display_name,
            config_abs.as_ref().map(|p| p.display().to_string()),
            if opts.auto_start { "Automatic" } else { "Manual" }
        );
        Ok(())
    }

    fn uninstall_service(name: &str) -> Result<()> {
        try_stop_and_delete(name).with_context(|| format!("uninstall service '{name}'"))?;
        info!("Uninstalled service '{}'", name);
        Ok(())
    }

    fn try_stop_and_delete(name: &str) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("opening Service Control Manager")?;

        let service = match manager.open_service(
            name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        ) {
            Ok(s) => s,
            Err(windows_service::Error::Winapi(io))
                if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
            {
                return Ok(());
            }
            Err(e) => return Err(anyhow!("open_service: {e}")),
        };

        let status = service
            .query_status()
            .map_err(|e| anyhow!("query_status: {e}"))?;
        if status.current_state != ServiceState::Stopped
            && status.current_state != ServiceState::StopPending
        {
            let _ = service.stop();
        }

        let deadline = std::time::Instant::now() + FORCE_STOP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match service.query_status() {
                Ok(s) if s.current_state == ServiceState::Stopped => break,
                Ok(_) => std::thread::sleep(Duration::from_millis(250)),
                Err(_) => break,
            }
        }

        service
            .delete()
            .map_err(|e| anyhow!("delete service: {e}"))?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // SCM service dispatcher (entry point for mechanism=service installs)
    // ------------------------------------------------------------------

    struct ServiceCtx {
        name: String,
        config: Option<PathBuf>,
    }

    static SERVICE_CTX: OnceLock<ServiceCtx> = OnceLock::new();

    pub fn run_service(name: String, config: Option<PathBuf>) -> Result<()> {
        SERVICE_CTX
            .set(ServiceCtx {
                name: name.clone(),
                config,
            })
            .map_err(|_| anyhow!("service context already initialized"))?;

        service_dispatcher::start(name.as_str(), ffi_service_main)
            .map_err(|e| anyhow!("service_dispatcher::start failed: {e}"))?;
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = service_body() {
            tracing::error!("service exited with error: {e:#}");
        }
    }

    fn service_body() -> Result<()> {
        let ctx = SERVICE_CTX
            .get()
            .ok_or_else(|| anyhow!("SERVICE_CTX not populated before dispatcher start"))?;

        let shutdown = CancellationToken::new();
        let shutdown_handler = shutdown.clone();

        let status_handle =
            service_control_handler::register(ctx.name.as_str(), move |control| match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    shutdown_handler.cancel();
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            })
            .map_err(|e| anyhow!("service_control_handler::register: {e}"))?;

        set_status(
            &status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            0,
        )?;

        let config_path = crate::resolve_config_path(ctx.config.clone());
        info!(
            "service '{}' starting with config {}",
            ctx.name,
            config_path.display()
        );

        let result = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for service")
            .and_then(|rt| rt.block_on(crate::run_main(config_path, shutdown)));

        let exit_code = match &result {
            Ok(()) => ServiceExitCode::Win32(0),
            Err(e) => {
                warn!("service loop ended with error: {e:#}");
                ServiceExitCode::Win32(1)
            }
        };

        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });

        result
    }

    fn set_status(
        handle: &service_control_handler::ServiceStatusHandle,
        state: ServiceState,
        accept: ServiceControlAccept,
        checkpoint: u32,
    ) -> Result<()> {
        handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .map_err(|e| anyhow!("set_service_status: {e}"))
    }

    // ------------------------------------------------------------------
    // Shared helpers
    // ------------------------------------------------------------------

    fn absolute_config_path(p: &Path) -> Result<PathBuf> {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()?.join(p)
        };
        match std::fs::canonicalize(&abs) {
            Ok(c) => Ok(canonical_no_verbatim(&c)),
            Err(_) => Ok(abs),
        }
    }

    /// Strip the `\\?\` verbatim prefix that `canonicalize` emits on Windows.
    /// Both SCM and Task Scheduler store the binary path literally, and the
    /// `\\?\` form looks odd in their UIs and breaks some downstream tools.
    fn canonical_no_verbatim(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            PathBuf::from(stripped)
        } else {
            p.to_path_buf()
        }
    }
}
