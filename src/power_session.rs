//! Sleep / shutdown / session lock hooks so the device screen can power down and
//! come back after resume or login (mirajazz `Device::sleep`).
//!
//! Windows: suspend/resume via `RegisterSuspendResumeNotification`, workstation
//! lock/unlock and logon via `WTSRegisterSessionNotification`. Other
//! platforms: no-op stub.

use std::{
    sync::{Arc, Mutex as StdMutex},
    thread::JoinHandle,
};

use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSessionEvent {
    /// Turn key displays off (sleep / lock / shutdown).
    DisplayOff,
    /// Restore brightness and redraw the current page.
    DisplayOn,
}

/// Shared state between the spawner and the message-pump thread. Covers both
/// the "shutdown requested before the window exists" race and the regular
/// "post WM_CLOSE to the running window" path.
#[derive(Debug, Default)]
struct WatcherShared {
    /// `Some(hwnd as isize)` once `CreateWindowExW` succeeded.
    hwnd: Option<isize>,
    /// Set by `PowerWatcher::request_shutdown`. The message-pump checks it
    /// right after window creation so a request that arrived during setup
    /// still triggers teardown.
    shutdown_requested: bool,
}

/// Handle returned by `spawn_watcher`; the caller keeps it alive while the
/// app runs and calls `request_shutdown` + `join` at teardown.
pub struct PowerWatcher {
    #[cfg_attr(not(windows), allow(dead_code))]
    join: JoinHandle<()>,
    #[cfg_attr(not(windows), allow(dead_code))]
    shared: Arc<StdMutex<WatcherShared>>,
}

impl PowerWatcher {
    /// Ask the watcher thread to exit. Safe to call before window creation
    /// finishes — the flag is latched and checked by the message-pump.
    pub fn request_shutdown(&self) {
        #[cfg(windows)]
        windows_imp::request_shutdown(&self.shared);
        #[cfg(not(windows))]
        let _ = &self.shared;
    }

    /// Block the calling thread until the watcher thread exits. Intended to
    /// be called from `tokio::task::spawn_blocking`.
    pub fn join(self) {
        let _ = self.join.join();
    }
}

/// Background thread; drops its send half when the message loop exits.
pub fn spawn_watcher(tx: UnboundedSender<PowerSessionEvent>) -> PowerWatcher {
    let shared = Arc::new(StdMutex::new(WatcherShared::default()));
    #[cfg(windows)]
    let join = {
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = windows_imp::run_message_thread(tx, shared) {
                warn!(error = ?e, "power/session notification thread ended with error");
            }
        })
    };
    #[cfg(not(windows))]
    let join = {
        drop(tx);
        std::thread::spawn(|| {})
    };
    PowerWatcher { join, shared }
}

#[cfg(windows)]
mod windows_imp {
    use std::{
        ffi::c_void,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    use tokio::sync::mpsc::UnboundedSender;
    use tracing::warn;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Power::{
                DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY,
                RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification,
            },
            RemoteDesktop::{
                NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
                WTSUnRegisterSessionNotification,
            },
        },
        UI::WindowsAndMessaging::{
            CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DEVICE_NOTIFY_CALLBACK,
            DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
            GetWindowLongPtrW, HMENU, HWND_MESSAGE, MSG, PBT_APMRESUMEAUTOMATIC,
            PBT_APMRESUMECRITICAL, PBT_APMRESUMESTANDBY, PBT_APMRESUMESUSPEND, PBT_APMSTANDBY,
            PBT_APMSUSPEND, PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowLongPtrW,
            TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE,
            WM_DESTROY, WM_ENDSESSION, WM_NCCREATE, WM_POWERBROADCAST, WM_QUERYENDSESSION,
            WM_WTSSESSION_CHANGE, WNDCLASSEXW, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF,
            WTS_SESSION_LOGON, WTS_SESSION_UNLOCK,
        },
    };
    use windows::core::HSTRING;

    use super::{PowerSessionEvent, WatcherShared};

    static CLASS_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Data handed to `SetWindowLongPtrW(GWLP_USERDATA)`; accessed from the
    /// OS-owned callback thread (power notifications) and the message-pump
    /// thread (window proc). All fields are `Send + Sync`.
    ///
    /// `session_locked` gates [`PowerSessionEvent::DisplayOn`] on power-resume:
    /// when Windows is configured to require a password on wakeup, a suspend
    /// fires `WTS_SESSION_LOCK` around the same time as `PBT_APMSUSPEND`, and
    /// on resume we get `PBT_APMRESUME*` first (session still locked),
    /// followed by `WTS_SESSION_UNLOCK` only after the user authenticates.
    /// Restoring the device on the lock screen is undesirable, so we only
    /// emit `DisplayOn` from the power path when this flag is `false`; the
    /// session path always emits it and flips the flag.
    ///
    /// Initialised to `false` — the app runs inside an interactive user
    /// session which, by definition, starts unlocked.
    struct UserData {
        tx: UnboundedSender<PowerSessionEvent>,
        session_locked: AtomicBool,
    }

    impl UserData {
        /// Send `DisplayOn` unconditionally (session path). Any prior lock is
        /// obviously over if we just received `UNLOCK` / `LOGON`.
        fn emit_display_on_unlock(&self) {
            self.session_locked.store(false, Ordering::Release);
            let _ = self.tx.send(PowerSessionEvent::DisplayOn);
        }

        /// Mark session locked and turn display off (session path).
        fn emit_display_off_lock(&self) {
            self.session_locked.store(true, Ordering::Release);
            let _ = self.tx.send(PowerSessionEvent::DisplayOff);
        }

        /// Power-path resume: suppress `DisplayOn` if a lock is pending —
        /// the device should stay off until `WTS_SESSION_UNLOCK` fires.
        fn emit_display_on_if_unlocked(&self) {
            if !self.session_locked.load(Ordering::Acquire) {
                let _ = self.tx.send(PowerSessionEvent::DisplayOn);
            }
        }

        /// Power-path suspend: display off regardless of lock state.
        fn emit_display_off_suspend(&self) {
            let _ = self.tx.send(PowerSessionEvent::DisplayOff);
        }
    }

    pub fn request_shutdown(shared: &Arc<StdMutex<WatcherShared>>) {
        let Ok(mut guard) = shared.lock() else {
            return;
        };
        guard.shutdown_requested = true;
        if let Some(hwnd_val) = guard.hwnd {
            let hwnd = HWND(hwnd_val as *mut c_void);
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn run_message_thread(
        tx: UnboundedSender<PowerSessionEvent>,
        shared: Arc<StdMutex<WatcherShared>>,
    ) -> windows::core::Result<()> {
        let ud = Box::new(UserData {
            tx,
            session_locked: AtomicBool::new(false),
        });
        let raw_ud = Box::into_raw(ud);

        let mut subscribe = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_callback),
            Context: raw_ud as *mut c_void,
        };

        // Registering for suspend/resume notifications. The DLL keeps its own
        // copy of the callback + context, so `subscribe` can live on the stack.
        let power_handle: Option<HPOWERNOTIFY> = match unsafe {
            RegisterSuspendResumeNotification(
                windows::Win32::Foundation::HANDLE(
                    &mut subscribe as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void,
                ),
                DEVICE_NOTIFY_CALLBACK,
            )
        } {
            Ok(h) => Some(h),
            Err(e) => {
                warn!(
                    error = ?e,
                    "RegisterSuspendResumeNotification failed; relying on WM_POWERBROADCAST if any"
                );
                None
            }
        };

        let class_name = HSTRING::from(format!(
            "n4ctl_pwr_{}_{}",
            std::process::id(),
            CLASS_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let hinstance: windows::Win32::Foundation::HINSTANCE =
            unsafe { GetModuleHandleW(None)? }.into();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: windows::core::PCWSTR(class_name.as_wide().as_ptr()),
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&wc) } == 0 {
            cleanup_power(power_handle);
            unsafe {
                drop(Box::from_raw(raw_ud));
            }
            return Err(windows::core::Error::from_win32());
        }

        let hwnd = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                &class_name,
                &HSTRING::from(""),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                HMENU::default(),
                hinstance,
                Some(raw_ud as *const c_void),
            )
        } {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    let _ = UnregisterClassW(&class_name, hinstance);
                }
                cleanup_power(power_handle);
                unsafe {
                    drop(Box::from_raw(raw_ud));
                }
                return Err(e);
            }
        };

        // Publish the hwnd; if shutdown was requested during setup, skip the
        // pump entirely and go straight to cleanup.
        let skip_pump = {
            let mut guard = shared.lock().expect("watcher shared poisoned");
            guard.hwnd = Some(hwnd.0 as isize);
            guard.shutdown_requested
        };

        if !skip_pump {
            if let Err(e) = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
            {
                warn!(error = ?e, "WTSRegisterSessionNotification failed; lock/unlock hooks disabled");
            }

            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        } else {
            // Window still exists (wndproc will unregister WTS in WM_DESTROY);
            // tear it down synchronously.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }

        // Clear the hwnd now that the pump has drained and the window is gone.
        if let Ok(mut guard) = shared.lock() {
            guard.hwnd = None;
        }

        cleanup_power(power_handle);
        unsafe {
            let _ = UnregisterClassW(&class_name, hinstance);
            drop(Box::from_raw(raw_ud));
        }

        Ok(())
    }

    fn cleanup_power(power: Option<HPOWERNOTIFY>) {
        if let Some(h) = power {
            unsafe {
                let _ = UnregisterSuspendResumeNotification(h);
            }
        }
    }

    unsafe extern "system" fn power_callback(
        context: *const c_void,
        ty: u32,
        _setting: *const c_void,
    ) -> u32 {
        if context.is_null() {
            return 0;
        }
        let ud = unsafe { &*(context as *const UserData) };
        match ty {
            PBT_APMSUSPEND | PBT_APMSTANDBY => ud.emit_display_off_suspend(),
            PBT_APMRESUMESUSPEND
            | PBT_APMRESUMESTANDBY
            | PBT_APMRESUMEAUTOMATIC
            | PBT_APMRESUMECRITICAL => ud.emit_display_on_if_unlocked(),
            _ => {}
        }
        0
    }

    /// Retrieve the `UserData` pointer stashed in `GWLP_USERDATA` during
    /// `WM_NCCREATE`. Returns `None` between window creation and that message
    /// (extremely narrow window, but `DefWindowProcW` may fire pre-NCCREATE
    /// messages with `USERDATA == 0`).
    fn user_data(hwnd: HWND) -> Option<&'static UserData> {
        let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const UserData;
        if raw.is_null() {
            return None;
        }
        Some(unsafe { &*raw })
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCCREATE => {
                let cs = lparam.0 as *const CREATESTRUCTW;
                if cs.is_null() {
                    return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
                }
                let raw = unsafe { (*cs).lpCreateParams } as *mut UserData;
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
                }
                LRESULT(1)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    if let Some(ud) = user_data(hwnd) {
                        ud.emit_display_off_suspend();
                    }
                }
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_POWERBROADCAST => {
                if let Some(ud) = user_data(hwnd) {
                    match wparam.0 as u32 {
                        PBT_APMSUSPEND | PBT_APMSTANDBY => ud.emit_display_off_suspend(),
                        PBT_APMRESUMESUSPEND
                        | PBT_APMRESUMESTANDBY
                        | PBT_APMRESUMEAUTOMATIC
                        | PBT_APMRESUMECRITICAL => ud.emit_display_on_if_unlocked(),
                        _ => {}
                    }
                }
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_WTSSESSION_CHANGE => {
                if let Some(ud) = user_data(hwnd) {
                    match wparam.0 as u32 {
                        WTS_SESSION_LOCK | WTS_SESSION_LOGOFF => ud.emit_display_off_lock(),
                        WTS_SESSION_UNLOCK | WTS_SESSION_LOGON => ud.emit_display_on_unlock(),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = unsafe { DestroyWindow(hwnd) };
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = unsafe { WTSUnRegisterSessionNotification(hwnd) };
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }
}
