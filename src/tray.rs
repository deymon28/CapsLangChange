//! Tray icon, menu and all UI-thread logic (configuration reloads, exclusions, Caps Lock restore,
//! shortcut recorder, autostart management).

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use toml_edit::DocumentMut;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, GlobalFree, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, SEE_MASK_NOASYNC,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, Shell_NotifyIconW, ShellExecuteExW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::autostart::{self, Mode};
use crate::config::{self, Config, Method};
use crate::hook::{self, HookHandle};
use crate::inject;
use crate::layout;
use crate::log;
use crate::process;
use crate::util::{copy_to_fixed, wide};
use crate::watcher;

pub const WINDOW_CLASS: &str = "CapsLangChange.Window";
pub const WINDOW_TITLE: &str = "CapsLangChange";
pub const ALREADY_RUNNING_MESSAGE: &str = "CapsLangChange.AlreadyRunning";
const REPO_URL: &str = "https://github.com/deymon28/CapsLangChange";

const MSG_TRAY: u32 = WM_APP + 50;
const MSG_FOREGROUND: u32 = WM_APP + 51;
const MSG_HELPER_DONE: u32 = WM_APP + 52;

const TIMER_CAPTURE: usize = 1;
const TIMER_RECORD: usize = 2;
/// Re-checks the configuration and foreground after a notification arrived while the state was busy.
const TIMER_RETRY: usize = 3;
const CAPTURE_SECONDS: u32 = 5;
const RECORD_SECONDS: u32 = 10;

const ICON_ACTIVE: usize = 0;
const ICON_PAUSED: usize = 1;
const ICON_EXCLUDED: usize = 2;

// Menu command identifiers.
const CMD_ENABLED: usize = 1;
const CMD_METHOD_WIN_SPACE: usize = 2;
const CMD_METHOD_MESSAGE: usize = 3;
const CMD_CAPTURE: usize = 4;
const CMD_RECORD: usize = 5;
const CMD_RESTORE_CAPS: usize = 6;
const CMD_AUTOSTART_OFF: usize = 7;
const CMD_AUTOSTART_NORMAL: usize = 8;
const CMD_AUTOSTART_ELEVATED: usize = 9;
const CMD_OPEN_CONFIG: usize = 10;
const CMD_RELOAD: usize = 11;
const CMD_CHECK: usize = 12;
const CMD_OPEN_LOGS: usize = 13;
const CMD_LAYOUTS: usize = 14;
const CMD_ABOUT: usize = 15;
const CMD_EXIT: usize = 16;
const CMD_BINDING_BASE: usize = 1000;
const CMD_REMOVE_EXCLUDE_BASE: usize = 2000;
const CMD_ADD_PROGRAM_BASE: usize = 3000;
const CMD_RANGE: usize = 1000;

const HELPER_REGISTER: usize = 1;
const HELPER_UNREGISTER: usize = 2;
const HELPER_CANCELLED: u32 = 0xFFFF_FFFE;

pub struct Paths {
    pub config: PathBuf,
    pub logs: PathBuf,
    pub exe: PathBuf,
}

pub struct Startup {
    pub config_created: bool,
}

struct App {
    hwnd: HWND,
    hook: HookHandle,
    paths: Paths,
    config_text: String,
    config: Arc<Config>,
    icons: [HICON; 3],
    icon_state: usize,
    fg_exe: Option<String>,
    fg_excluded: bool,
    saved_caps: Option<bool>,
    msg_taskbar_created: u32,
    msg_already_running: u32,
    menu_programs: Vec<String>,
    autostart: Mode,
    pending_autostart: Option<Mode>,
    elevated: bool,
    capture_pending: bool,
    record_pending: bool,
    winevent: HWINEVENTHOOK,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

static UI_HWND: AtomicUsize = AtomicUsize::new(0);

/// Runs `f` with the application state unless it is already borrowed (re-entrant message).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| {
        let mut guard = a.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// Creates the window and tray icon, starts the hook and runs the message loop. Returns the exit code.
pub fn run(paths: Paths, config_text: String, config: Arc<Config>, startup: Startup) -> i32 {
    // SAFETY: standard window creation on this thread; all strings outlive the calls.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(WINDOW_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: LoadIconW(instance, 1 as _),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        if RegisterClassExW(&wc) == 0 {
            fatal(&format!("Cannot register the window class: {}", std::io::Error::last_os_error()));
            return 1;
        }
        // A hidden top-level window (not message-only) so Explorer's TaskbarCreated broadcast arrives.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            wide(WINDOW_TITLE).as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        fatal(&format!("Cannot create the main window: {}", std::io::Error::last_os_error()));
        return 1;
    }
    UI_HWND.store(hwnd as usize, Ordering::Relaxed);

    let hook = match HookHandle::start(Arc::clone(&config), hwnd) {
        Ok(h) => h,
        Err(e) => {
            log::error!("tray", "{e}");
            fatal(&format!("Cannot install the keyboard hook.\n\n{e}"));
            return 1;
        }
    };

    // SAFETY: plain registrations; strings outlive the calls.
    let (msg_taskbar_created, msg_already_running, winevent) = unsafe {
        let taskbar = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let running = RegisterWindowMessageW(wide(ALREADY_RUNNING_MESSAGE).as_ptr());
        // Allow these from lower-integrity senders when running elevated.
        ChangeWindowMessageFilterEx(hwnd, taskbar, MSGFLT_ALLOW, std::ptr::null_mut());
        ChangeWindowMessageFilterEx(hwnd, running, MSGFLT_ALLOW, std::ptr::null_mut());
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let winevent = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(foreground_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        (taskbar, running, winevent)
    };

    if let Some(dir) = paths.config.parent() {
        watcher::start(dir, hwnd);
    }

    let elevated = autostart::is_elevated();
    let app = App {
        hwnd,
        hook,
        paths,
        config_text,
        config,
        icons: load_icons(),
        icon_state: ICON_ACTIVE,
        fg_exe: None,
        fg_excluded: false,
        saved_caps: None,
        msg_taskbar_created,
        msg_already_running,
        menu_programs: Vec::new(),
        autostart: autostart::current_mode(),
        pending_autostart: None,
        elevated,
        capture_pending: false,
        record_pending: false,
        winevent,
    };
    APP.with(|a| *a.borrow_mut() = Some(app));

    with_app(|app| {
        app.add_icon();
        app.update_foreground(process::foreground_app(), false);
        app.startup_notices(&startup);
    });
    log::info!("tray", "running (elevated: {elevated})");

    // SAFETY: standard message loop.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    APP.with(|a| {
        if let Some(mut app) = a.borrow_mut().take() {
            app.remove_icon();
            // SAFETY: handles created above.
            unsafe {
                if !app.winevent.is_null() {
                    UnhookWinEvent(app.winevent);
                }
                WTSUnRegisterSessionNotification(app.hwnd);
            }
            app.hook.stop();
        }
    });
    log::info!("tray", "exited");
    0
}

/// Shows an error message box (used when the tray cannot start).
pub fn fatal(text: &str) {
    message_box(std::ptr::null_mut(), text, MB_OK | MB_ICONERROR);
}

fn message_box(hwnd: HWND, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    // SAFETY: strings outlive the call.
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide("CapsLangChange").as_ptr(), flags | MB_SETFOREGROUND) }
}

fn load_icons() -> [HICON; 3] {
    // SAFETY: icons are resources of this executable.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let cx = GetSystemMetrics(SM_CXSMICON);
        let cy = GetSystemMetrics(SM_CYSMICON);
        let load = |id: usize| LoadImageW(instance, id as _, IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) as HICON;
        [load(1), load(2), load(3)]
    }
}

unsafe extern "system" fn foreground_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let ui = UI_HWND.load(Ordering::Relaxed) as HWND;
    if !ui.is_null() {
        // SAFETY: plain message post.
        unsafe { PostMessageW(ui, MSG_FOREGROUND, 0, hwnd as LPARAM) };
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        MSG_TRAY => {
            let event = (lparam & 0xFFFF) as u32;
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT | NIN_KEYSELECT) {
                show_menu(hwnd);
            }
            0
        }
        MSG_FOREGROUND => {
            let app = process::app_for_window(lparam as HWND);
            if with_app(|a| a.update_foreground(app, true)).is_none() {
                retry_later(hwnd);
            }
            0
        }
        watcher::MSG_CONFIG_CHANGED => {
            if with_app(|a| a.reload(false)).is_none() {
                retry_later(hwnd);
            }
            0
        }
        hook::MSG_RECORDED => {
            // SAFETY: the box was created by the hook thread and is consumed exactly once.
            let text = unsafe { Box::from_raw(lparam as *mut String) };
            with_app(|a| a.recorded(&text));
            0
        }
        MSG_HELPER_DONE => {
            on_helper_done(wparam, lparam as u32);
            0
        }
        WM_TIMER => {
            match wparam {
                TIMER_CAPTURE => with_app(|a| a.capture_now()),
                TIMER_RECORD => with_app(|a| a.record_timeout()),
                TIMER_RETRY => {
                    // SAFETY: timer on our own window.
                    unsafe { KillTimer(hwnd, TIMER_RETRY) };
                    let done = with_app(|a| {
                        a.reload(false);
                        a.update_foreground(process::foreground_app(), true);
                    });
                    if done.is_none() {
                        retry_later(hwnd);
                    }
                    done
                }
                _ => None,
            };
            0
        }
        WM_WTSSESSION_CHANGE => {
            if wparam == WTS_SESSION_UNLOCK as usize {
                with_app(|a| a.hook.resync());
            }
            0
        }
        WM_POWERBROADCAST => {
            if wparam == PBT_APMRESUMEAUTOMATIC as usize {
                with_app(|a| a.hook.resync());
            }
            1
        }
        WM_DESTROY => {
            // SAFETY: ends the message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => {
            let handled = with_app(|a| {
                if msg == a.msg_taskbar_created && a.msg_taskbar_created != 0 {
                    a.add_icon();
                    true
                } else if msg == a.msg_already_running && a.msg_already_running != 0 {
                    a.notify("Already running", "CapsLangChange is already running here in the tray.", NIIF_INFO);
                    true
                } else {
                    false
                }
            });
            if handled == Some(true) {
                return 0;
            }
            // SAFETY: default processing.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

// Constants that windows-sys exposes under other features or names.
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1; // NIN_SELECT | NINF_KEY
const WTS_SESSION_UNLOCK: u32 = 0x8;
const PBT_APMRESUMEAUTOMATIC: u32 = 0x12;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;

fn retry_later(hwnd: HWND) {
    // SAFETY: timer on our own window; re-arming an existing timer just resets it.
    unsafe { SetTimer(hwnd, TIMER_RETRY, 300, None) };
}

fn show_menu(hwnd: HWND) {
    let Some(menu) = with_app(|a| a.build_menu()) else { return };
    // SAFETY: menu handle created above; the borrow of APP is released while the modal menu runs.
    let cmd = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd =
            TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd as usize
    };
    if cmd != 0 {
        command(cmd);
    }
}

fn command(cmd: usize) {
    match cmd {
        CMD_ENABLED => {
            with_app(|a| {
                let enabled = !a.config.enabled;
                a.edit(|doc| {
                    config::edit_set_bool(doc, "enabled", enabled);
                    true
                });
            });
        }
        CMD_METHOD_WIN_SPACE | CMD_METHOD_MESSAGE => {
            let method = if cmd == CMD_METHOD_WIN_SPACE { Method::WinSpace } else { Method::Message };
            with_app(|a| {
                a.edit(|doc| {
                    config::edit_set_method(doc, method);
                    true
                })
            });
        }
        CMD_RESTORE_CAPS => {
            with_app(|a| {
                let value = !a.config.restore_capslock_on_leave;
                a.edit(|doc| {
                    config::edit_set_bool(doc, "restore_capslock_on_leave", value);
                    true
                });
            });
        }
        CMD_CAPTURE => {
            with_app(|a| a.start_capture());
        }
        CMD_RECORD => {
            with_app(|a| a.start_record());
        }
        CMD_AUTOSTART_OFF => set_autostart(Mode::Off),
        CMD_AUTOSTART_NORMAL => set_autostart(Mode::Normal),
        CMD_AUTOSTART_ELEVATED => set_autostart(Mode::Elevated),
        // Shell calls may pump messages, so they run without holding the application state.
        CMD_OPEN_CONFIG => {
            if let Some((hwnd, path)) = with_app(|a| (a.hwnd, a.paths.config.clone())) {
                open_config(hwnd, &path);
            }
        }
        CMD_RELOAD => {
            with_app(|a| a.reload(true));
        }
        CMD_CHECK => check_config(),
        CMD_OPEN_LOGS => {
            if let Some((hwnd, logs)) = with_app(|a| (a.hwnd, a.paths.logs.clone())) {
                let _ = std::fs::create_dir_all(&logs);
                shell_open(hwnd, &logs.display().to_string());
            }
        }
        CMD_LAYOUTS => show_layouts(),
        CMD_ABOUT => show_about(),
        CMD_EXIT => {
            if let Some(hwnd) = with_app(|a| a.hwnd) {
                // SAFETY: our own window.
                unsafe { DestroyWindow(hwnd) };
            }
        }
        c if (CMD_BINDING_BASE..CMD_BINDING_BASE + CMD_RANGE).contains(&c) => {
            let index = c - CMD_BINDING_BASE;
            with_app(|a| {
                let enabled = a.config.bindings.iter().find(|b| b.index == index).map(|b| !b.enabled);
                if let Some(enabled) = enabled {
                    a.edit(|doc| config::edit_binding_enabled(doc, index, enabled));
                }
            });
        }
        c if (CMD_REMOVE_EXCLUDE_BASE..CMD_REMOVE_EXCLUDE_BASE + CMD_RANGE).contains(&c) => {
            let index = c - CMD_REMOVE_EXCLUDE_BASE;
            with_app(|a| a.edit(|doc| config::edit_remove_exclude(doc, index)));
        }
        c if (CMD_ADD_PROGRAM_BASE..CMD_ADD_PROGRAM_BASE + CMD_RANGE).contains(&c) => {
            with_app(|a| {
                if let Some(exe) = a.menu_programs.get(c - CMD_ADD_PROGRAM_BASE).cloned() {
                    a.add_exclude(&exe);
                }
            });
        }
        _ => {}
    }
}

impl App {
    fn icon_data(&self) -> NOTIFYICONDATAW {
        // SAFETY: zero is a valid initial state for this plain C struct.
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = 1;
        nid
    }

    fn tooltip(&self) -> String {
        let state = match self.icon_state {
            ICON_PAUSED => "paused".to_string(),
            ICON_EXCLUDED => format!("inactive in {}", self.fg_exe.as_deref().unwrap_or("this program")),
            _ => "active".to_string(),
        };
        let admin = if self.elevated { " (administrator)" } else { "" };
        format!("CapsLangChange: {state}{admin}")
    }

    fn add_icon(&mut self) {
        let mut nid = self.icon_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = MSG_TRAY;
        nid.hIcon = self.icons[self.icon_state];
        copy_to_fixed(&mut nid.szTip, &self.tooltip());
        // SAFETY: valid struct; re-adding after an Explorer restart is expected to succeed.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &nid);
            if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
                log::warning!("tray", "adding the tray icon failed");
            }
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    fn remove_icon(&mut self) {
        let nid = self.icon_data();
        // SAFETY: valid struct.
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }

    fn refresh_icon(&mut self) {
        let state = if !self.config.enabled {
            ICON_PAUSED
        } else if self.fg_excluded {
            ICON_EXCLUDED
        } else {
            ICON_ACTIVE
        };
        self.icon_state = state;
        let mut nid = self.icon_data();
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = self.icons[state];
        copy_to_fixed(&mut nid.szTip, &self.tooltip());
        // SAFETY: valid struct.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }

    fn notify(&self, title: &str, text: &str, flags: u32) {
        let mut nid = self.icon_data();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = flags;
        copy_to_fixed(&mut nid.szInfoTitle, title);
        copy_to_fixed(&mut nid.szInfo, text);
        // SAFETY: valid struct.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }

    fn startup_notices(&mut self, startup: &Startup) {
        if startup.config_created {
            self.notify(
                "CapsLangChange is running",
                "Caps Lock now switches the keyboard layout. Right-click the tray icon for settings.",
                NIIF_INFO,
            );
        }
        if !self.config.warnings.is_empty() {
            self.notify("Configuration warning", &self.config.warnings[0], NIIF_WARNING);
        }
        if process::is_running("PowerToys.KeyboardManagerEngine.exe") && !autostart::flag("KbmNoticeShown") {
            autostart::set_flag("KbmNoticeShown");
            log::info!("tray", "PowerToys Keyboard Manager is running; showed the one-time conflict notice");
            self.notify(
                "PowerToys Keyboard Manager is active",
                "If it also remaps Caps Lock, remove that remap there to avoid switching the layout twice.",
                NIIF_WARNING,
            );
        }
        autostart::refresh_run_value(&self.paths.exe);
    }

    fn update_foreground(&mut self, app: Option<config::AppInfo>, allow_restore: bool) {
        let excluded = app.as_ref().is_some_and(|a| self.config.is_excluded(a));
        if excluded && !self.fg_excluded {
            self.saved_caps = Some(inject::caps_on());
            log::debug!("tray", "entered excluded program {}", app.as_ref().map_or("?", |a| a.exe.as_str()));
        } else if !excluded && self.fg_excluded {
            if allow_restore && self.config.enabled && self.config.restore_capslock_on_leave {
                if let Some(saved) = self.saved_caps {
                    if inject::caps_on() != saved {
                        inject::caps_tap();
                        log::info!("tray", "restored Caps Lock to {} after leaving an excluded program", on_off(saved));
                    }
                }
            }
            self.saved_caps = None;
        }
        self.fg_excluded = excluded;
        self.fg_exe = app.map(|a| a.exe);
        self.refresh_icon();
    }

    /// Re-reads the configuration file; `force` reapplies even if the text is unchanged.
    fn reload(&mut self, force: bool) {
        let text = match std::fs::read_to_string(&self.paths.config) {
            Ok(t) => t,
            Err(e) => {
                if force {
                    self.notify("Cannot read the configuration", &e.to_string(), NIIF_ERROR);
                }
                return;
            }
        };
        if !force && text == self.config_text {
            return;
        }
        self.config_text = text;
        match config::parse(&self.config_text) {
            Ok(config) => {
                for w in &config.warnings {
                    log::warning!("config", "{w}");
                }
                if let Some(w) = config.warnings.first() {
                    self.notify("Configuration loaded with warnings", w, NIIF_WARNING);
                } else if force {
                    self.notify("Configuration reloaded", "The configuration is valid and active.", NIIF_INFO);
                }
                log::info!(
                    "config",
                    "configuration applied ({} bindings, {} exclusions)",
                    config.bindings.len(),
                    config.excludes.len()
                );
                self.apply(Arc::new(config));
            }
            Err(errors) => {
                for e in &errors {
                    log::error!("config", "{e}");
                }
                self.notify(
                    "Configuration error - previous settings stay active",
                    &errors.first().cloned().unwrap_or_default(),
                    NIIF_ERROR,
                );
            }
        }
    }

    fn apply(&mut self, config: Arc<Config>) {
        self.config = Arc::clone(&config);
        self.hook.set_config(config);
        let app = process::foreground_app();
        self.update_foreground(app, false);
    }

    /// Applies a comment-preserving edit to the file, then reloads.
    fn edit(&mut self, f: impl FnOnce(&mut DocumentMut) -> bool) {
        let text = match std::fs::read_to_string(&self.paths.config) {
            Ok(t) => t,
            Err(e) => {
                self.notify("Cannot read the configuration", &e.to_string(), NIIF_ERROR);
                return;
            }
        };
        let mut doc: DocumentMut = match text.parse() {
            Ok(d) => d,
            Err(_) => {
                self.notify(
                    "Configuration has a syntax error",
                    "Fix the file first (tray menu > Check config) before changing settings here.",
                    NIIF_ERROR,
                );
                return;
            }
        };
        if !f(&mut doc) {
            return;
        }
        if let Err(e) = config::write_atomic(&self.paths.config, &doc.to_string()) {
            self.notify("Cannot save the configuration", &e.to_string(), NIIF_ERROR);
            return;
        }
        self.reload(false);
    }

    fn add_exclude(&mut self, exe: &str) {
        let mut added = false;
        self.edit(|doc| {
            added = config::edit_add_exclude(doc, exe);
            added
        });
        if added {
            log::info!("tray", "excluded program added: {exe}");
            self.notify("Program excluded", &format!("{exe} now receives all keys unchanged."), NIIF_INFO);
        } else {
            self.notify("Already excluded", &format!("{exe} is already in the list."), NIIF_INFO);
        }
    }

    fn start_capture(&mut self) {
        self.capture_pending = true;
        // SAFETY: timer on our own window.
        unsafe { SetTimer(self.hwnd, TIMER_CAPTURE, CAPTURE_SECONDS * 1000, None) };
        self.notify(
            "Switch to the program to exclude",
            &format!("The program that is active in {CAPTURE_SECONDS} seconds will be added to the exclusions."),
            NIIF_INFO,
        );
    }

    fn capture_now(&mut self) {
        // SAFETY: timer on our own window.
        unsafe { KillTimer(self.hwnd, TIMER_CAPTURE) };
        if !std::mem::take(&mut self.capture_pending) {
            return;
        }
        match process::foreground_app() {
            Some(app) if app.path != self.paths.exe.display().to_string().to_lowercase() => {
                let exe = original_case_exe(&app.path).unwrap_or(app.exe);
                self.add_exclude(&exe);
            }
            _ => self.notify(
                "Nothing captured",
                "Could not identify the active program. Try again, or add it in the config file.",
                NIIF_WARNING,
            ),
        }
    }

    fn start_record(&mut self) {
        self.record_pending = true;
        self.hook.record(true);
        // SAFETY: timer on our own window.
        unsafe { SetTimer(self.hwnd, TIMER_RECORD, RECORD_SECONDS * 1000, None) };
        self.notify(
            "Press the shortcut to record",
            &format!("Its text for the config file will be copied to the clipboard (waiting {RECORD_SECONDS} s)."),
            NIIF_INFO,
        );
    }

    fn recorded(&mut self, text: &str) {
        // SAFETY: timer on our own window.
        unsafe { KillTimer(self.hwnd, TIMER_RECORD) };
        self.record_pending = false;
        if set_clipboard(self.hwnd, text) {
            self.notify("Shortcut recorded", &format!("{text}  (copied to the clipboard)"), NIIF_INFO);
        } else {
            self.notify("Shortcut recorded", text, NIIF_INFO);
        }
    }

    fn record_timeout(&mut self) {
        // SAFETY: timer on our own window.
        unsafe { KillTimer(self.hwnd, TIMER_RECORD) };
        if std::mem::take(&mut self.record_pending) {
            self.hook.record(false);
            self.notify("Nothing recorded", "No shortcut was pressed.", NIIF_INFO);
        }
    }

    fn build_menu(&mut self) -> HMENU {
        // SAFETY: menus are created here and destroyed by the caller (submenus with their parent).
        unsafe {
            let menu = CreatePopupMenu();
            let item = |m: HMENU, id: usize, text: &str, checked: bool, enabled: bool| {
                let mut flags = MF_STRING;
                if checked {
                    flags |= MF_CHECKED;
                }
                if !enabled {
                    flags |= MF_GRAYED;
                }
                AppendMenuW(m, flags, id, wide(text).as_ptr());
            };
            let separator = |m: HMENU| {
                AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            };
            let submenu = |m: HMENU, sub: HMENU, text: &str| {
                AppendMenuW(m, MF_POPUP, sub as usize, wide(text).as_ptr());
            };

            item(menu, CMD_ENABLED, "Enabled", self.config.enabled, true);
            separator(menu);

            let bindings = CreatePopupMenu();
            if self.config.bindings.is_empty() {
                item(bindings, 0, "No bindings in the config file", false, false);
            }
            for b in self.config.bindings.iter().take(CMD_RANGE) {
                item(bindings, CMD_BINDING_BASE + b.index, &menu_text(&b.label()), b.enabled, true);
            }
            submenu(menu, bindings, "Bindings");

            let method = CreatePopupMenu();
            item(method, CMD_METHOD_WIN_SPACE, "Win+Space (default)", self.config.method == Method::WinSpace, true);
            item(method, CMD_METHOD_MESSAGE, "Windows API message", self.config.method == Method::Message, true);
            submenu(menu, method, "Layout switch method");

            let excluded = CreatePopupMenu();
            if self.config.excludes.is_empty() {
                item(excluded, 0, "No excluded programs", false, false);
            }
            for (i, m) in self.config.excludes.iter().enumerate().take(CMD_RANGE) {
                item(excluded, CMD_REMOVE_EXCLUDE_BASE + i, &menu_text(&format!("Remove {}", m.label())), false, true);
            }
            separator(excluded);
            self.menu_programs = process::windowed_programs();
            let running = CreatePopupMenu();
            if self.menu_programs.is_empty() {
                item(running, 0, "No programs with windows found", false, false);
            }
            for (i, exe) in self.menu_programs.iter().enumerate().take(CMD_RANGE) {
                item(running, CMD_ADD_PROGRAM_BASE + i, &menu_text(exe), false, true);
            }
            submenu(excluded, running, "Add running program");
            item(
                excluded,
                CMD_CAPTURE,
                &format!("Add the program active in {CAPTURE_SECONDS} seconds"),
                false,
                !self.capture_pending,
            );
            submenu(menu, excluded, "Excluded programs");

            item(menu, CMD_RECORD, "Record a shortcut...", false, !self.record_pending);
            item(
                menu,
                CMD_RESTORE_CAPS,
                "Restore Caps Lock after excluded programs",
                self.config.restore_capslock_on_leave,
                true,
            );

            let start = CreatePopupMenu();
            let busy = self.pending_autostart.is_some();
            item(start, CMD_AUTOSTART_OFF, "Off", self.autostart == Mode::Off, !busy);
            item(start, CMD_AUTOSTART_NORMAL, "Normal", self.autostart == Mode::Normal, !busy);
            item(
                start,
                CMD_AUTOSTART_ELEVATED,
                "As administrator (works in admin windows)",
                self.autostart == Mode::Elevated,
                !busy,
            );
            submenu(menu, start, "Start with Windows");
            separator(menu);

            item(menu, CMD_OPEN_CONFIG, "Open config file", false, true);
            item(menu, CMD_RELOAD, "Reload config", false, true);
            item(menu, CMD_CHECK, "Check config", false, true);
            item(menu, CMD_LAYOUTS, "Installed layouts", false, true);
            item(menu, CMD_OPEN_LOGS, "Open logs folder", false, true);
            item(menu, CMD_ABOUT, "About", false, true);
            separator(menu);
            item(menu, CMD_EXIT, "Exit", false, true);
            menu
        }
    }
}

fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// Escapes '&' (menu accelerator marker) and limits length.
fn menu_text(text: &str) -> String {
    let mut t: String = text.chars().take(80).collect();
    if text.chars().count() > 80 {
        t.push('…');
    }
    t.replace('&', "&&")
}

/// File name with its original letter case, for nicer config entries.
fn original_case_exe(lower_path: &str) -> Option<String> {
    let path = std::path::Path::new(lower_path);
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy().to_string();
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.eq_ignore_ascii_case(&name))
}

fn open_config(hwnd: HWND, path: &std::path::Path) {
    let path = path.display().to_string();
    if !shell_open(hwnd, &path) {
        // No program is associated with .toml files: fall back to Notepad.
        // SAFETY: strings outlive the call.
        unsafe {
            ShellExecuteW(
                hwnd,
                wide("open").as_ptr(),
                wide("notepad.exe").as_ptr(),
                wide(format!("\"{path}\"")).as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

fn shell_open(hwnd: HWND, target: &str) -> bool {
    // SAFETY: strings outlive the call.
    let r = unsafe {
        ShellExecuteW(
            hwnd,
            wide("open").as_ptr(),
            wide(target).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    let data = wide(text);
    let bytes = data.len() * 2;
    // SAFETY: the global block is sized for the data; ownership passes to the clipboard on success.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        let mut ok = false;
        if !mem.is_null() {
            let ptr = GlobalLock(mem) as *mut u16;
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
                GlobalUnlock(mem);
                ok = !SetClipboardData(13 /* CF_UNICODETEXT */, mem).is_null();
            }
            if !ok {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
        ok
    }
}

fn check_config() {
    let Some((hwnd, path)) = with_app(|a| (a.hwnd, a.paths.config.clone())) else { return };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            message_box(hwnd, &format!("Cannot read {}:\n{e}", path.display()), MB_OK | MB_ICONERROR);
            return;
        }
    };
    match config::parse(&text) {
        Ok(c) if c.warnings.is_empty() => {
            message_box(
                hwnd,
                &format!(
                    "The configuration is valid.\n\n{} bindings, {} excluded programs.",
                    c.bindings.len(),
                    c.excludes.len()
                ),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        Ok(c) => {
            message_box(
                hwnd,
                &format!("The configuration is valid, with warnings:\n\n{}", c.warnings.join("\n")),
                MB_OK | MB_ICONWARNING,
            );
        }
        Err(errors) => {
            message_box(
                hwnd,
                &format!("The configuration has errors (the previous settings stay active):\n\n{}", errors.join("\n")),
                MB_OK | MB_ICONERROR,
            );
        }
    }
}

fn show_layouts() {
    let Some(hwnd) = with_app(|a| a.hwnd) else { return };
    let lines = layout::describe_installed();
    let text = if lines.is_empty() {
        "No keyboard layouts found.".to_string()
    } else {
        format!(
            "Installed keyboard layouts (in switching order):\n\n{}\n\nUse these ids in the config, e.g.\nlayout = \"{}\" or cycle = [...].",
            lines.join("\n"),
            lines[0].split_whitespace().next().unwrap_or("00000409")
        )
    };
    message_box(hwnd, &text, MB_OK | MB_ICONINFORMATION);
}

fn show_about() {
    let Some((hwnd, text)) = with_app(|a| {
        let text = format!(
            "CapsLangChange {}\n\nCaps Lock layout switching and per-program keyboard shortcuts.\n\n\
             Running as administrator: {}\nConfig: {}\nLogs: {}\n\n{REPO_URL}",
            env!("CARGO_PKG_VERSION"),
            if a.elevated { "yes" } else { "no" },
            a.paths.config.display(),
            a.paths.logs.display(),
        );
        (a.hwnd, text)
    }) else {
        return;
    };
    message_box(hwnd, &text, MB_OK | MB_ICONINFORMATION);
}

// ---------------------------------------------------------------------------------------------
// Autostart.

fn set_autostart(mode: Mode) {
    let Some((hwnd, current, elevated, exe)) = with_app(|a| (a.hwnd, a.autostart, a.elevated, a.paths.exe.clone()))
    else {
        return;
    };
    if mode == current {
        return;
    }

    if mode == Mode::Elevated {
        if autostart::exe_in_user_profile(&exe) {
            let answer = message_box(
                hwnd,
                &format!(
                    "CapsLangChange.exe is inside your user profile:\n{}\n\nAny program running as you could \
                     replace it there, and it would then start with administrator rights. A safer place is \
                     C:\\Program Files\\CapsLangChange.\n\nEnable administrator autostart anyway?",
                    exe.display()
                ),
                MB_YESNO | MB_ICONWARNING,
            );
            if answer != IDYES {
                return;
            }
        }
        let Some(sid) = autostart::current_user_sid() else {
            message_box(hwnd, "Cannot determine the current user.", MB_OK | MB_ICONERROR);
            return;
        };
        if elevated {
            // Already administrator: create the task directly, no restart needed.
            match autostart::register_task(&exe, &sid, false) {
                Ok(()) => finish_autostart(Mode::Elevated),
                Err(e) => {
                    message_box(hwnd, &format!("Could not enable administrator autostart:\n{e}"), MB_OK | MB_ICONERROR);
                }
            }
        } else {
            with_app(|a| a.pending_autostart = Some(Mode::Elevated));
            run_helper(hwnd, exe, format!("--register-task {sid}"), HELPER_REGISTER);
        }
        return;
    }

    // Off or Normal: remove the scheduled task first if there is one.
    if current == Mode::Elevated || autostart::task_exists() {
        if elevated {
            if let Err(e) = autostart::unregister_task() {
                message_box(hwnd, &format!("Could not remove the administrator task:\n{e}"), MB_OK | MB_ICONERROR);
                return;
            }
        } else {
            with_app(|a| a.pending_autostart = Some(mode));
            run_helper(hwnd, exe, "--unregister-task".to_string(), HELPER_UNREGISTER);
            return;
        }
    }
    finish_autostart(mode);
}

fn finish_autostart(mode: Mode) {
    let result = with_app(|a| {
        a.pending_autostart = None;
        let result = match mode {
            Mode::Normal => autostart::enable_run_value(&a.paths.exe).map_err(|e| e.to_string()),
            _ => {
                autostart::disable_run_value();
                Ok(())
            }
        };
        if result.is_ok() {
            autostart::remember_mode(mode);
            a.autostart = mode;
            log::info!("autostart", "autostart set to {mode:?}");
        }
        (a.hwnd, result)
    });
    if let Some((hwnd, Err(e))) = result {
        message_box(hwnd, &format!("Could not change autostart:\n{e}"), MB_OK | MB_ICONERROR);
    }
}

/// Runs this executable elevated with `args` on a worker thread (UAC prompt) and reports the exit code.
fn run_helper(hwnd: HWND, exe: PathBuf, args: String, kind: usize) {
    let hwnd_addr = hwnd as usize;
    std::thread::spawn(move || {
        let code = run_elevated(&exe, &args);
        // SAFETY: plain message post to our window.
        unsafe { PostMessageW(hwnd_addr as HWND, MSG_HELPER_DONE, kind, code as LPARAM) };
    });
}

fn run_elevated(exe: &std::path::Path, args: &str) -> u32 {
    let verb = wide("runas");
    let file = wide(exe);
    let params = wide(args);
    // SAFETY: strings outlive the call; the process handle is waited on and closed.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            return if GetLastError() == 1223 { HELPER_CANCELLED } else { 1 };
        }
        if info.hProcess.is_null() {
            return 1;
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        code
    }
}

fn on_helper_done(kind: usize, code: u32) {
    let Some((hwnd, pending)) = with_app(|a| (a.hwnd, a.pending_autostart)) else { return };
    let Some(mode) = pending else { return };
    if code != 0 {
        with_app(|a| a.pending_autostart = None);
        if code != HELPER_CANCELLED {
            log::error!("autostart", "elevated helper failed with exit code {code}");
            message_box(
                hwnd,
                "Changing the administrator autostart failed. See the log for details.",
                MB_OK | MB_ICONERROR,
            );
        }
        return;
    }
    finish_autostart(mode);
    if kind == HELPER_REGISTER {
        // The scheduled task has started an elevated instance; it takes over when this one exits.
        log::info!("autostart", "handing over to the elevated instance");
        // SAFETY: our own window.
        unsafe { DestroyWindow(hwnd) };
    }
}
