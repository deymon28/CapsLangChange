//! Low-level keyboard hook thread: feeds events to the engine and executes its decisions.
//!
//! The thread owns the hook and a plain message loop, so the keyboard never waits for the tray UI.
//! Other threads talk to it only through thread messages (`HookHandle`).

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PM_NOREMOVE, PeekMessageW,
    PostMessageW, PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP,
    WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_TIMER,
};

use crate::config::{Action, CapsState, Config, LayoutMode, Method};
use crate::engine::{Decision, Engine, Origin};
use crate::inject::{self, TAG_BASE, TAG_MASK, TAG_RESTORE};
use crate::keys::{MODIFIER_VKS, ModPattern, Shortcut, modifier_bit};
use crate::{layout, log, process};

const MSG_CONFIG: u32 = WM_APP + 1;
const MSG_RECORD: u32 = WM_APP + 2;
const MSG_RESYNC: u32 = WM_APP + 3;

/// Posted to the UI window when the recorder captured a shortcut; `lParam` is a `Box<String>`.
pub const MSG_RECORDED: u32 = WM_APP + 40;

const WATCHDOG_MS: u32 = 60_000;
/// Re-install the hook if no key event arrived for this long while the user was active.
const SILENCE_MS: u32 = 10 * 60_000;

struct Ctx {
    engine: Engine,
    hook: HHOOK,
    ui: HWND,
    recording: bool,
    last_event_tick: u32,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

pub struct HookHandle {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HookHandle {
    /// Starts the hook thread and waits until the hook is installed.
    pub fn start(config: Arc<Config>, ui: HWND) -> Result<HookHandle, String> {
        let (tx, rx) = mpsc::channel::<Result<u32, String>>();
        let ui_addr = ui as usize;
        let join = std::thread::Builder::new()
            .name("keyboard-hook".into())
            .spawn(move || run(config, ui_addr as HWND, tx))
            .map_err(|e| format!("cannot start the hook thread: {e}"))?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Ok(HookHandle { thread_id, join: Some(join) }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => Err("the hook thread exited during start-up".into()),
        }
    }

    pub fn set_config(&self, config: Arc<Config>) {
        let raw = Box::into_raw(Box::new(config));
        // SAFETY: on success the hook thread takes ownership of the box; on failure it is reclaimed here.
        unsafe {
            if PostThreadMessageW(self.thread_id, MSG_CONFIG, 0, raw as LPARAM) == 0 {
                drop(Box::from_raw(raw));
                log::error!("hook", "could not deliver the new configuration to the hook thread");
            }
        }
    }

    pub fn record(&self, start: bool) {
        // SAFETY: plain message post.
        unsafe { PostThreadMessageW(self.thread_id, MSG_RECORD, start as WPARAM, 0) };
    }

    /// Re-reads the modifier state and re-installs the hook (after unlock or resume).
    pub fn resync(&self) {
        // SAFETY: plain message post.
        unsafe { PostThreadMessageW(self.thread_id, MSG_RESYNC, 0, 0) };
    }

    pub fn stop(&mut self) {
        // SAFETY: plain message post.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn install() -> HHOOK {
    // SAFETY: the hook procedure is a static function of this module.
    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), GetModuleHandleW(std::ptr::null()), 0) }
}

fn run(config: Arc<Config>, ui: HWND, ready: mpsc::Sender<Result<u32, String>>) {
    // SAFETY: thread-local Win32 calls on this thread.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        // Create the message queue before reporting readiness, so posted messages are not lost.
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    layout::warm_cache();
    let mut engine = Engine::new(config);
    engine.set_physical(read_held());
    let hook = install();
    if hook.is_null() {
        let _ = ready.send(Err(format!("SetWindowsHookExW failed: {}", std::io::Error::last_os_error())));
        return;
    }
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx { engine, hook, ui, recording: false, last_event_tick: unsafe { GetTickCount() } })
    });
    // SAFETY: plain call.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    log::info!("hook", "keyboard hook installed");

    // SAFETY: thread timer without a window; WM_TIMER arrives in this thread's queue.
    unsafe { SetTimer(std::ptr::null_mut(), 0, WATCHDOG_MS, None) };

    loop {
        // SAFETY: standard message loop on this thread.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r <= 0 {
            break;
        }
        match msg.message {
            MSG_CONFIG => {
                // SAFETY: the box was created by `HookHandle::set_config` and is consumed exactly once.
                let config = unsafe { Box::from_raw(msg.lParam as *mut Arc<Config>) };
                with_ctx(|ctx| ctx.engine.set_config(*config));
            }
            MSG_RECORD => with_ctx(|ctx| ctx.recording = msg.wParam != 0),
            MSG_RESYNC => reinstall("session change"),
            WM_TIMER => watchdog(),
            _ => {}
        }
    }

    CTX.with(|c| {
        if let Some(ctx) = c.borrow_mut().take() {
            // SAFETY: the hook handle belongs to this thread.
            unsafe { UnhookWindowsHookEx(ctx.hook) };
        }
    });
    log::info!("hook", "keyboard hook removed");
}

fn with_ctx(f: impl FnOnce(&mut Ctx)) {
    CTX.with(|c| {
        if let Ok(mut guard) = c.try_borrow_mut() {
            if let Some(ctx) = guard.as_mut() {
                f(ctx);
            }
        }
    });
}

fn reinstall(reason: &str) {
    with_ctx(|ctx| {
        // SAFETY: the old handle belongs to this thread; a failed unhook (already removed) is harmless.
        unsafe { UnhookWindowsHookEx(ctx.hook) };
        let hook = install();
        if hook.is_null() {
            log::error!("hook", "re-installing the hook failed ({reason}): {}", std::io::Error::last_os_error());
        } else {
            ctx.hook = hook;
            log::info!("hook", "hook re-installed ({reason})");
        }
        ctx.engine.set_physical(read_held());
        // SAFETY: plain call.
        ctx.last_event_tick = unsafe { GetTickCount() };
    });
}

/// Windows silently removes a hook that once exceeded the system time-out. If the user is active
/// but no key event reached us for a long time, re-install the hook (harmless if it was fine).
fn watchdog() {
    let mut silent = false;
    with_ctx(|ctx| {
        let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        // SAFETY: plain queries.
        let (now, ok) = unsafe { (GetTickCount(), GetLastInputInfo(&mut info) != 0) };
        let user_active = ok && now.wrapping_sub(info.dwTime) < WATCHDOG_MS;
        silent = user_active && now.wrapping_sub(ctx.last_event_tick) > SILENCE_MS;
    });
    if silent {
        reinstall("watchdog");
    }
}

/// Logical state of all eight modifier keys.
fn read_held() -> u8 {
    let mut bits = 0;
    for (bit, vk) in MODIFIER_VKS {
        // SAFETY: plain query.
        if unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0 {
            bits |= bit;
        }
    }
    bits
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT owned by the system.
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if handle(kb, wparam) {
            return 1;
        }
    }
    // SAFETY: forwarding the unchanged arguments.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Returns true to swallow the event.
fn handle(kb: &KBDLLHOOKSTRUCT, wparam: WPARAM) -> bool {
    let vk = (kb.vkCode & 0xFF) as u8;
    let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let origin = if kb.flags & LLKHF_INJECTED != 0 && kb.dwExtraInfo & TAG_MASK == TAG_BASE {
        if kb.dwExtraInfo == TAG_RESTORE { Origin::OursRestore } else { Origin::Ours }
    } else {
        Origin::External
    };

    CTX.with(|c| {
        // A busy context would mean re-entrancy; let the event through rather than risk anything.
        let Ok(mut guard) = c.try_borrow_mut() else { return false };
        let Some(ctx) = guard.as_mut() else { return false };

        if origin == Origin::External {
            ctx.last_event_tick = kb.time;
            if ctx.recording && down && modifier_bit(vk).is_none() {
                ctx.recording = false;
                ctx.engine.mark_swallowed(vk);
                let sc = Shortcut { mods: ModPattern::from_held(read_held()), vk };
                let text = Box::into_raw(Box::new(sc.to_string()));
                // SAFETY: the UI thread takes ownership on success; reclaimed on failure.
                unsafe {
                    if PostMessageW(ctx.ui, MSG_RECORDED, 0, text as LPARAM) == 0 {
                        drop(Box::from_raw(text));
                    }
                }
                return true;
            }
        }

        let decision = ctx.engine.on_key(vk, kb.scanCode, down, origin, read_held, process::foreground_app_cached);
        match decision {
            Decision::Pass => false,
            Decision::Swallow { mask, run, held } => {
                if mask {
                    inject::mask();
                }
                if let Some(index) = run {
                    let config = Arc::clone(ctx.engine.config());
                    if let Some(binding) = config.bindings.get(index) {
                        log::debug!("hook", "binding {} fired: {}", index + 1, binding.label());
                        execute(&binding.action, held);
                    }
                }
                true
            }
        }
    })
}

fn execute(action: &Action, held: u8) {
    match action {
        Action::SwitchLayout { mode, cycle, method } => match (method, mode) {
            (Method::WinSpace, LayoutMode::Next) => inject::win_space(false, held),
            (Method::WinSpace, LayoutMode::Previous) => inject::win_space(true, held),
            _ => layout::switch_by_message(*mode, cycle),
        },
        Action::CapsLock(state) => {
            let want = match state {
                CapsState::Toggle => None,
                CapsState::On => Some(true),
                CapsState::Off => Some(false),
            };
            if want.is_none_or(|w| w != inject::caps_on()) {
                inject::caps_tap();
            }
        }
        Action::SendKeys(sc) => inject::shortcut(sc, held),
        Action::Block | Action::Passthrough => {}
    }
}
