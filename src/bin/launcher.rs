//! `n4ctl-launcher`: zero-flash autostart stub.
//!
//! This is a tiny GUI-subsystem (`windows_subsystem = "windows"`) binary whose
//! only job is to spawn `n4ctl.exe` (which lives next to us) with
//! `CREATE_NO_WINDOW` so the console subsystem child never gets an allocated
//! console window. Task Scheduler launches this instead of `n4ctl.exe`
//! directly, giving a flicker-free login experience while keeping `n4ctl.exe`
//! itself a normal console program for interactive CLI use.
//!
//! We intentionally `wait()` on the child rather than detaching:
//! * Task Scheduler shows the task state as "Running" for as long as
//!   `n4ctl.exe` is alive.
//! * Stopping the task (`schtasks /End /TN n4ctl`) terminates the task's
//!   job object, which kills this process *and* the child.
//! * The launcher's exit code mirrors the child's, so task history shows
//!   meaningful last-run results.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("n4ctl-launcher is only meaningful on Windows; run n4ctl directly instead.");
    std::process::ExitCode::from(1)
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, ExitCode};

    // MSDN: "CREATE_NO_WINDOW: The process is a console application that is
    // being run without a console window." No console is allocated and no
    // window is ever shown, which is exactly what we need.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let target = match locate_n4ctl() {
        Ok(p) => p,
        Err(_) => return ExitCode::from(1),
    };

    let mut cmd = Command::new(&target);
    cmd.args(std::env::args_os().skip(1));
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return ExitCode::from(1),
    };

    match child.wait() {
        Ok(status) => status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .map(ExitCode::from)
            .unwrap_or(ExitCode::from(1)),
        Err(_) => ExitCode::from(1),
    }
}

#[cfg(windows)]
fn locate_n4ctl() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "launcher has no parent dir"))?;
    let target = dir.join("n4ctl.exe");
    if !target.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("n4ctl.exe not found next to launcher: {}", target.display()),
        ));
    }
    Ok(target)
}
