//! Language indicator: a small capsule next to the text cursor showing the language a binding switched to.
//!
//! It runs on its own thread with its own window, so a slow caret query (accessibility calls into other
//! programs) never delays the keyboard hook or the tray. The hook only posts messages to it (`Notifier`).
//!
//! The capsule is drawn into a per-pixel-alpha bitmap (`UpdateLayeredWindow`); the window is click-through,
//! never activated and topmost. While it is visible a short timer follows the foreground layout (switching
//! with Win+Space takes effect asynchronously), the trigger key's release and the fade-out.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, DeleteDC, DeleteObject, FW_SEMIBOLD, GdiFlush, GetDC, GetMonitorInfoW,
    GetTextExtentPoint32W, HBITMAP, HDC, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::UI::Accessibility::AccessibleObjectFromWindow;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetWindowDpiAwarenessContext, LogicalToPhysicalPointForPerMonitorDPI, MDT_EFFECTIVE_DPI,
    SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GUITHREADINFO, GetCursorPos,
    GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowRect, GetWindowThreadProcessId, HWND_TOPMOST,
    KillTimer, MA_NOACTIVATE, MSG, OBJID_CARET, PostMessageW, PostQuitMessage, RegisterClassExW, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetTimer, SetWindowPos, ShowWindow, ULW_ALPHA,
    UpdateLayeredWindow, WM_APP, WM_CLOSE, WM_DESTROY, WM_MOUSEACTIVATE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::config::{Config, IndicatorPosition};
use crate::util::wide;
use crate::{layout, log};

const WINDOW_CLASS: &str = "CapsLangChange.Indicator";

/// A binding switched the layout. `wParam`: the trigger key while it is held (0 for a modifier tap);
/// `lParam`: the foreground layout before the switch.
const MSG_SWITCHED: u32 = WM_APP + 1;
/// Auto-repeat of the held trigger key.
const MSG_REPEAT: u32 = WM_APP + 2;
const MSG_RELEASED: u32 = WM_APP + 3;
/// New configuration; `lParam` is a `Box<Arc<Config>>`.
const MSG_CONFIG: u32 = WM_APP + 4;

const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 30;
/// Longest wait for the layout change to reach the foreground program before showing what it has.
const WAIT_MS: u32 = 300;
/// A held key auto-repeats; without a repeat for this long its release was missed (hook re-installed).
const HELD_TIMEOUT_MS: u32 = 2000;
const FADE_STEP: u8 = 255 / 5;

/// Sends events to the indicator thread. Cheap to copy; does nothing if the indicator is not running.
#[derive(Clone, Copy)]
pub struct Notifier(usize);

impl Notifier {
    /// A binding switched the layout. `held` is the trigger key if it is still held down (0 for a modifier
    /// tap), `before` the foreground layout before the switch.
    pub fn switched(&self, held: u8, before: HKL) {
        self.post(MSG_SWITCHED, held as WPARAM, before as LPARAM);
    }

    pub fn repeat(&self) {
        self.post(MSG_REPEAT, 0, 0);
    }

    pub fn released(&self) {
        self.post(MSG_RELEASED, 0, 0);
    }

    fn post(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) {
        if self.0 != 0 {
            // SAFETY: plain message post; never waits for the receiver.
            unsafe { PostMessageW(self.0 as HWND, msg, wparam, lparam) };
        }
    }
}

pub struct IndicatorHandle {
    hwnd: usize,
    done: Option<mpsc::Receiver<()>>,
}

impl IndicatorHandle {
    /// Starts the indicator thread. If that fails, the program works without the indicator.
    pub fn start(config: Arc<Config>) -> IndicatorHandle {
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let spawned = std::thread::Builder::new().name("indicator".into()).spawn(move || {
            run(config, ready_tx);
            let _ = done_tx.send(());
        });
        let hwnd = match spawned {
            Ok(_) => ready_rx.recv().unwrap_or(0),
            Err(e) => {
                log::warning!("indicator", "cannot start the indicator thread: {e}");
                0
            }
        };
        IndicatorHandle { hwnd, done: (hwnd != 0).then_some(done_rx) }
    }

    pub fn notifier(&self) -> Notifier {
        Notifier(self.hwnd)
    }

    pub fn set_config(&self, config: Arc<Config>) {
        if self.hwnd == 0 {
            return;
        }
        let raw = Box::into_raw(Box::new(config));
        // SAFETY: on success the indicator thread takes ownership of the box; on failure it is reclaimed here.
        unsafe {
            if PostMessageW(self.hwnd as HWND, MSG_CONFIG, 0, raw as LPARAM) == 0 {
                drop(Box::from_raw(raw));
            }
        }
    }

    /// Closes the window and waits briefly for the thread. A caret query stuck in a hung program must not
    /// keep the process from exiting, so the thread is not joined.
    pub fn stop(&mut self) {
        let Some(done) = self.done.take() else { return };
        // SAFETY: plain message post.
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        let _ = done.recv_timeout(Duration::from_secs(1));
    }
}

impl Drop for IndicatorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------
// Indicator thread.

struct State {
    hwnd: HWND,
    config: Arc<Config>,
    /// Waiting for the switch to reach the foreground program: the layout before it and since when.
    pending: Option<(HKL, u32)>,
    /// The trigger key is still held.
    held: bool,
    last_key: u32,
    released: u32,
    /// Where the capsule goes; fixed while it is visible, so repeated switches do not move it.
    anchor: Option<Anchor>,
    /// The anchor is being looked up; nothing is shown until it is known.
    locating: bool,
    /// Text on screen and when it was shown.
    shown: Option<(String, u32)>,
    alpha: u8,
    surface: Option<Surface>,
    timer: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn run(config: Arc<Config>, ready: mpsc::Sender<usize>) {
    // SAFETY: COM for the accessibility caret query, then standard window creation on this thread.
    let hwnd = unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(WINDOW_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            wide("").as_ptr(),
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
        log::warning!("indicator", "cannot create the indicator window: {}", std::io::Error::last_os_error());
        let _ = ready.send(0);
        // SAFETY: balances CoInitializeEx above.
        unsafe { CoUninitialize() };
        return;
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            hwnd,
            config,
            pending: None,
            held: false,
            last_key: 0,
            released: 0,
            anchor: None,
            locating: false,
            shown: None,
            alpha: 0,
            surface: None,
            timer: false,
        })
    });
    let _ = ready.send(hwnd as usize);

    // SAFETY: standard message loop on this thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
    STATE.with(|s| s.borrow_mut().take());
    // SAFETY: balances CoInitializeEx above.
    unsafe { CoUninitialize() };
}

fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with(|s| {
        if let Ok(mut guard) = s.try_borrow_mut() {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        MSG_SWITCHED => {
            let mut locate = None;
            with_state(|s| locate = s.switched(wparam as u8, lparam as HKL));
            // The accessibility query may dispatch messages (a cross-process COM call), so it runs without
            // holding the state: a key release arriving meanwhile is not lost.
            if let Some(position) = locate {
                let anchor = find_anchor(position);
                with_state(|s| {
                    s.locating = false;
                    s.anchor = Some(anchor);
                    s.tick();
                });
            }
        }
        MSG_REPEAT => with_state(|s| s.last_key = now()),
        MSG_RELEASED => with_state(|s| s.release()),
        MSG_CONFIG => {
            // SAFETY: the box was created by `IndicatorHandle::set_config` and is consumed exactly once.
            let config = unsafe { Box::from_raw(lparam as *mut Arc<Config>) };
            with_state(|s| {
                s.config = *config;
                if !s.config.indicator.enabled {
                    s.hide();
                }
            });
        }
        WM_TIMER if wparam == TIMER_TICK => with_state(|s| s.tick()),
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_DESTROY => {
            with_state(|s| s.hide());
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
        WM_CLOSE => {
            // SAFETY: our own window.
            unsafe { DestroyWindow(hwnd) };
        }
        // SAFETY: default processing.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}

fn now() -> u32 {
    // SAFETY: plain call.
    unsafe { GetTickCount() }
}

impl State {
    /// Returns the position setting if the anchor must be looked up before anything is shown.
    fn switched(&mut self, held: u8, before: HKL) -> Option<IndicatorPosition> {
        if !self.config.indicator.enabled {
            return None;
        }
        let now = now();
        self.held = held != 0;
        self.last_key = now;
        self.released = now;
        self.pending = Some((before, now));
        if !self.timer {
            // SAFETY: timer on our own window.
            unsafe { SetTimer(self.hwnd, TIMER_TICK, TICK_MS, None) };
            self.timer = true;
        }
        if self.shown.is_none() {
            self.locating = true;
            return Some(self.config.indicator.position);
        }
        self.tick();
        None
    }

    fn release(&mut self) {
        if self.held {
            self.held = false;
            self.released = now();
        }
    }

    fn tick(&mut self) {
        let now = now();
        let current = layout::foreground_layout();
        if let Some((before, since)) = self.pending {
            if self.locating {
                // Shown once the anchor is known.
            } else if current != before || now.wrapping_sub(since) >= WAIT_MS {
                self.pending = None;
                self.show(current, now, true);
            }
        } else if self.shown.is_some() {
            // The layout may change once more (a slow program, another switch).
            self.show(current, now, false);
        }
        if self.held && now.wrapping_sub(self.last_key) > HELD_TIMEOUT_MS {
            self.release();
        }
        if let Some((_, shown_at)) = &self.shown {
            let duration = self.config.indicator.duration_ms;
            let expired = now.wrapping_sub(*shown_at) >= duration && now.wrapping_sub(self.released) >= duration;
            if self.pending.is_none() && !self.held && expired {
                self.alpha = self.alpha.saturating_sub(FADE_STEP);
                if self.alpha == 0 {
                    self.hide();
                } else {
                    self.present();
                }
            }
        }
        if self.shown.is_none() && self.pending.is_none() && self.timer {
            // SAFETY: timer on our own window.
            unsafe { KillTimer(self.hwnd, TIMER_TICK) };
            self.timer = false;
        }
    }

    /// Shows `hkl`'s name. `fresh`: right after a switch, so the display time starts again.
    fn show(&mut self, hkl: HKL, now: u32, fresh: bool) {
        let text = layout::short_name(hkl, &self.config.indicator);
        let same = self.shown.as_ref().is_some_and(|(t, _)| *t == text);
        if same && !fresh {
            return;
        }
        let shown_at = if fresh { now } else { self.shown.as_ref().map_or(now, |(_, at)| *at) };
        if !same || self.surface.is_none() {
            let anchor = self.anchor.unwrap_or(Anchor::Mouse(cursor_pos()));
            self.surface = render(&text, anchor, self.config.indicator.height);
        }
        if self.surface.is_none() {
            self.hide();
            return;
        }
        let first = self.shown.is_none();
        self.shown = Some((text, shown_at));
        if fresh || !same {
            self.alpha = 255;
        }
        self.present();
        if first {
            // SAFETY: our own window; shown without taking the focus and kept above other windows.
            unsafe {
                ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn present(&self) {
        let Some(s) = &self.surface else { return };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: self.alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let origin = POINT { x: 0, y: 0 };
        // SAFETY: the surface's DC holds a premultiplied 32-bit bitmap of `size`.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            UpdateLayeredWindow(self.hwnd, screen, &s.pos, &s.size, s.dc, &origin, 0, &blend, ULW_ALPHA);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
    }

    fn hide(&mut self) {
        if self.shown.take().is_some() {
            // SAFETY: our own window.
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
        self.surface = None;
        self.anchor = None;
        self.locating = false;
        self.pending = None;
        self.held = false;
        self.alpha = 0;
    }
}

// ---------------------------------------------------------------------------------------------
// Where to show it.

#[derive(Clone, Copy)]
enum Anchor {
    /// Text cursor rectangle in screen coordinates.
    Caret(RECT),
    Mouse(POINT),
}

fn find_anchor(position: IndicatorPosition) -> Anchor {
    if position == IndicatorPosition::Caret {
        if let Some(rect) = caret_rect() {
            return Anchor::Caret(rect);
        }
    }
    Anchor::Mouse(cursor_pos())
}

fn cursor_pos() -> POINT {
    let mut pt = POINT { x: 0, y: 0 };
    // SAFETY: plain query.
    unsafe { GetCursorPos(&mut pt) };
    pt
}

/// The text cursor of the foreground window: the system caret, else the accessibility caret (browsers,
/// Electron and other programs that draw their own caret). Only accepted inside the foreground window.
fn caret_rect() -> Option<RECT> {
    // SAFETY: plain queries; handles come from the system.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return None;
        }
        let mut frame = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        GetWindowRect(foreground, &mut frame);
        let inside =
            |r: &RECT| r.left >= frame.left && r.left <= frame.right && r.top >= frame.top && r.top <= frame.bottom;

        let thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return None;
        }
        if !info.hwndCaret.is_null() && info.rcCaret.bottom > info.rcCaret.top {
            let rect = client_to_screen(info.hwndCaret, info.rcCaret);
            if inside(&rect) {
                return Some(rect);
            }
        }
        let target = if info.hwndFocus.is_null() { foreground } else { info.hwndFocus };
        accessible_caret(target).filter(inside)
    }
}

/// Converts a client rectangle of a window that may be DPI-unaware into physical screen coordinates.
unsafe fn client_to_screen(hwnd: HWND, r: RECT) -> RECT {
    let mut a = POINT { x: r.left, y: r.top };
    let mut b = POINT { x: r.right, y: r.bottom };
    // SAFETY: plain conversions; the thread's DPI context is restored right away.
    unsafe {
        let old = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd));
        ClientToScreen(hwnd, &mut a);
        ClientToScreen(hwnd, &mut b);
        if !old.is_null() {
            SetThreadDpiAwarenessContext(old);
        }
        LogicalToPhysicalPointForPerMonitorDPI(hwnd, &mut a);
        LogicalToPhysicalPointForPerMonitorDPI(hwnd, &mut b);
    }
    RECT { left: a.x, top: a.y, right: b.x, bottom: b.y }
}

/// `VARIANT` holding `VT_I4` (64-bit layout).
#[repr(C)]
struct Variant {
    vt: u16,
    reserved: [u16; 3],
    value: [usize; 2],
}

const VT_I4: u16 = 3;
const IID_IACCESSIBLE: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x618736e0_3c3d_11cf_810c_00aa00389b71);
/// `IUnknown::Release` and `IAccessible::accLocation` in the `IAccessible` vtable.
const SLOT_RELEASE: usize = 2;
const SLOT_ACC_LOCATION: usize = 22;

type Release = unsafe extern "system" fn(*mut c_void) -> u32;
/// The `VARIANT` argument is passed by value in COM; on x64 and ARM64 a 24-byte struct passed by value is
/// passed as a pointer to a caller-owned copy, which is what this signature does.
type AccLocation = unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32, *mut i32, *mut i32, *mut Variant) -> i32;

/// Location of the caret that the window reports through Microsoft Active Accessibility.
#[cfg(target_pointer_width = "64")]
unsafe fn accessible_caret(hwnd: HWND) -> Option<RECT> {
    let mut acc: *mut c_void = std::ptr::null_mut();
    // SAFETY: `acc` is a valid IAccessible on success; the vtable slots follow the published interface
    // layout, and the reference is released exactly once.
    unsafe {
        if AccessibleObjectFromWindow(hwnd, OBJID_CARET as u32, &IID_IACCESSIBLE, &mut acc) < 0 || acc.is_null() {
            return None;
        }
        let vtable = *(acc as *const *const usize);
        let acc_location: AccLocation = std::mem::transmute(*vtable.add(SLOT_ACC_LOCATION));
        let release: Release = std::mem::transmute(*vtable.add(SLOT_RELEASE));
        let mut child = Variant { vt: VT_I4, reserved: [0; 3], value: [CHILDID_SELF as usize, 0] };
        let (mut x, mut y, mut w, mut h) = (0, 0, 0, 0);
        let hr = acc_location(acc, &mut x, &mut y, &mut w, &mut h, &mut child);
        release(acc);
        (hr >= 0 && h > 0).then_some(RECT { left: x, top: y, right: x + w, bottom: y + h })
    }
}

#[cfg(not(target_pointer_width = "64"))]
unsafe fn accessible_caret(_hwnd: HWND) -> Option<RECT> {
    None
}

/// Top-left corner for a `w` x `h` box next to `anchor`: below it (left-aligned), or above it when there is no
/// room below, and always inside `work`.
fn place(anchor: RECT, gap: i32, w: i32, h: i32, work: RECT) -> (i32, i32) {
    let mut y = anchor.bottom + gap;
    if y + h > work.bottom {
        y = anchor.top - gap - h;
    }
    let x = anchor.left.min(work.right - w).max(work.left);
    (x, y.min(work.bottom - h).max(work.top))
}

// ---------------------------------------------------------------------------------------------
// Drawing.

/// A rendered capsule: a memory DC with a premultiplied 32-bit bitmap, and where it goes on screen.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    size: SIZE,
    pos: POINT,
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: objects created by `render` and owned by this surface.
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct Theme {
    fill: [f32; 4],
    border: [f32; 4],
    text: [f32; 3],
    shadow: f32,
}

const DARK: Theme =
    Theme { fill: [0.17, 0.17, 0.17, 0.96], border: [1.0, 1.0, 1.0, 0.12], text: [1.0, 1.0, 1.0], shadow: 0.30 };
const LIGHT: Theme =
    Theme { fill: [0.985, 0.985, 0.985, 0.97], border: [0.0, 0.0, 0.0, 0.10], text: [0.10, 0.10, 0.10], shadow: 0.18 };

/// Proportions relative to the configured height.
/// Font size: half the height plus a constant, so small boxes keep readable letters.
const FONT: f32 = 0.5;
const FONT_EXTRA: f32 = 2.5;
const PADDING: f32 = 0.43;
const MIN_WIDTH: f32 = 1.6;
const MARGIN: f32 = 0.27;

fn light_theme() -> bool {
    let mut value = 0u32;
    let mut size = 4u32;
    // SAFETY: a DWORD-sized buffer and its size are passed.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr(),
            wide("SystemUsesLightTheme").as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    rc == ERROR_SUCCESS && value != 0
}

/// Draws the capsule for `text`, `base_height` pixels high at 96 DPI, next to `anchor`.
fn render(text: &str, anchor: Anchor, base_height: u32) -> Option<Surface> {
    let reference = match anchor {
        Anchor::Caret(r) => POINT { x: r.left, y: r.bottom },
        Anchor::Mouse(p) => p,
    };
    // SAFETY: plain queries; the structs are sized before the calls.
    let (work, scale) = unsafe {
        let monitor = MonitorFromPoint(reference, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(monitor, &mut info);
        let (mut dpi_x, mut dpi_y) = (96, 96);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        (info.rcWork, dpi_x.max(96) as f32 / 96.0)
    };
    let px = |v: f32| (v * scale).round() as i32;
    let h = base_height as f32;
    let theme = if light_theme() { &LIGHT } else { &DARK };

    let (mask, mask_w, mask_h) = text_mask(text, px(h * FONT + FONT_EXTRA))?;
    let (ix0, iy0, ix1, iy1) = ink_bounds(&mask, mask_w, mask_h).unwrap_or((0, 0, mask_w, mask_h));
    let (ink_w, ink_h) = (ix1 - ix0, iy1 - iy0);

    let height = px(h);
    let width = (ink_w as i32 + 2 * px(h * PADDING)).max(px(h * MIN_WIDTH));
    let margin = px((h * MARGIN).max(3.0));
    let size = SIZE { cx: width + 2 * margin, cy: height + 2 * margin };

    let (anchor_rect, gap) = match anchor {
        Anchor::Caret(r) => (r, px((h * 0.1).max(2.0))),
        Anchor::Mouse(p) => (RECT { left: p.x + px(12.0), top: p.y - px(4.0), right: p.x, bottom: p.y + px(20.0) }, 0),
    };
    let (x, y) = place(anchor_rect, gap, width, height, work);
    let pos = POINT { x: x - margin, y: y - margin };

    // Text offset that centres the ink (not the font cell) in the capsule.
    let tx = margin as f32 + (width as f32 - ink_w as f32) / 2.0 - ix0 as f32;
    let ty = margin as f32 + (height as f32 - ink_h as f32) / 2.0 - iy0 as f32;
    let (tx, ty) = (tx.round() as i32, ty.round() as i32);

    let shape = Capsule { x: margin as f32, y: margin as f32, w: width as f32, h: height as f32 };
    let border = scale.max(1.0);
    let blur = (margin as f32 - scale).max(1.0);
    let shadow_dy = scale;

    // SAFETY: the DIB section is sized `size`, 32 bits per pixel, top-down; the pixel slice covers it exactly.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap =
            CreateDIBSection(dc, &dib_info(size.cx, size.cy), DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bitmap);
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, (size.cx * size.cy * 4) as usize);
        for y in 0..size.cy {
            for x in 0..size.cx {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = shape.distance(fx, fy);
                let fill = (0.5 - d).clamp(0.0, 1.0);
                let inner = (0.5 - (d + border)).clamp(0.0, 1.0);
                let ds = shape.distance(fx, fy - shadow_dy);
                let shadow = (1.0 - (ds + 1.0) / blur).clamp(0.0, 1.0);
                let mx = x - tx;
                let my = y - ty;
                let ink = if mx >= 0 && my >= 0 && (mx as usize) < mask_w && (my as usize) < mask_h {
                    mask[my as usize * mask_w + mx as usize] as f32 / 255.0
                } else {
                    0.0
                };

                let mut c = [0.0f32; 4];
                over(&mut c, [0.0, 0.0, 0.0], shadow * shadow * theme.shadow);
                over(&mut c, [theme.fill[0], theme.fill[1], theme.fill[2]], fill * theme.fill[3]);
                over(&mut c, [theme.border[0], theme.border[1], theme.border[2]], (fill - inner) * theme.border[3]);
                over(&mut c, theme.text, ink * fill);
                let i = ((y * size.cx + x) * 4) as usize;
                pixels[i] = (c[2] * 255.0).round() as u8;
                pixels[i + 1] = (c[1] * 255.0).round() as u8;
                pixels[i + 2] = (c[0] * 255.0).round() as u8;
                pixels[i + 3] = (c[3] * 255.0).round() as u8;
            }
        }
        GdiFlush();
        Some(Surface { dc, bitmap, old, size, pos })
    }
}

fn dib_info(w: i32, h: i32) -> BITMAPINFO {
    // SAFETY: zero is a valid initial state for this plain C struct.
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..info.bmiHeader
    };
    info
}

/// Renders `text` with grayscale anti-aliasing and returns its coverage (0-255) per pixel, with the size.
fn text_mask(text: &str, font_px: i32) -> Option<(Vec<u8>, usize, usize)> {
    let chars: Vec<u16> = text.encode_utf16().collect();
    // SAFETY: GDI objects are created, selected, restored and deleted here; the DIB pixels are read after
    // GdiFlush and within the bitmap's size.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        let font = CreateFontW(
            -font_px,
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide("Segoe UI").as_ptr(),
        );
        let old_font = SelectObject(dc, font);
        let mut extent = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, chars.as_ptr(), chars.len() as i32, &mut extent);
        let pad = 2;
        let (w, h) = (extent.cx.max(1) + 2 * pad, extent.cy.max(1) + 2 * pad);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(dc, &dib_info(w, h), DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        let mut result = None;
        if !bitmap.is_null() && !bits.is_null() {
            let old_bitmap = SelectObject(dc, bitmap);
            std::ptr::write_bytes(bits as *mut u8, 0, (w * h * 4) as usize);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FF_FFFF);
            TextOutW(dc, pad, pad, chars.as_ptr(), chars.len() as i32);
            GdiFlush();
            let pixels = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let mask = pixels.chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2])).collect();
            result = Some((mask, w as usize, h as usize));
            SelectObject(dc, old_bitmap);
            DeleteObject(bitmap);
        }
        SelectObject(dc, old_font);
        DeleteObject(font);
        DeleteDC(dc);
        result
    }
}

/// Bounding box `(x0, y0, x1, y1)` (end exclusive) of the pixels with any coverage.
fn ink_bounds(mask: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for y in 0..h {
        for x in 0..w {
            if mask[y * w + x] != 0 {
                let b = bounds.get_or_insert((x, y, x + 1, y + 1));
                b.0 = b.0.min(x);
                b.1 = b.1.min(y);
                b.2 = b.2.max(x + 1);
                b.3 = b.3.max(y + 1);
            }
        }
    }
    bounds
}

/// A horizontal capsule (fully rounded ends).
struct Capsule {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Capsule {
    /// Signed distance from the outline: negative inside.
    fn distance(&self, px: f32, py: f32) -> f32 {
        let r = self.h / 2.0;
        let cx = px.clamp(self.x + r, self.x + self.w - r);
        let cy = self.y + r;
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
    }
}

/// Composites `color` with coverage `alpha` over the premultiplied `dst`.
fn over(dst: &mut [f32; 4], color: [f32; 3], alpha: f32) {
    let keep = 1.0 - alpha;
    for i in 0..3 {
        dst[i] = color[i] * alpha + dst[i] * keep;
    }
    dst[3] = alpha + dst[3] * keep;
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: RECT = RECT { left: 0, top: 0, right: 1920, bottom: 1040 };

    #[test]
    fn placed_below_or_above_and_inside_the_work_area() {
        let caret = RECT { left: 300, top: 200, right: 301, bottom: 218 };
        assert_eq!(place(caret, 3, 48, 30, WORK), (300, 221));
        // No room below: above the caret.
        let low = RECT { left: 300, top: 1000, right: 301, bottom: 1018 };
        assert_eq!(place(low, 3, 48, 30, WORK), (300, 967));
        // At the right edge: shifted left.
        let right = RECT { left: 1900, top: 200, right: 1901, bottom: 218 };
        assert_eq!(place(right, 3, 48, 30, WORK), (1872, 221));
        // A second monitor left of the primary one.
        let work = RECT { left: -1280, top: 0, right: 0, bottom: 1024 };
        let caret = RECT { left: -1290, top: 10, right: -1289, bottom: 28 };
        assert_eq!(place(caret, 3, 48, 30, work), (-1280, 31));
    }

    #[test]
    fn capsule_distance() {
        let c = Capsule { x: 0.0, y: 0.0, w: 50.0, h: 20.0 };
        assert!(c.distance(25.0, 10.0) < -9.0);
        assert!((c.distance(25.0, 0.0)).abs() < 1e-4);
        assert!((c.distance(0.0, 10.0)).abs() < 1e-4);
        assert!((c.distance(60.0, 10.0) - 10.0).abs() < 1e-4);
        // The corners are round: the bounding box corner is outside.
        assert!(c.distance(0.5, 0.5) > 2.0);
    }

    #[test]
    fn ink_bounds_of_mask() {
        let mut mask = vec![0u8; 6 * 4];
        assert_eq!(ink_bounds(&mask, 6, 4), None);
        mask[6 + 2] = 10;
        mask[2 * 6 + 4] = 255;
        assert_eq!(ink_bounds(&mask, 6, 4), Some((2, 1, 5, 3)));
    }

    #[test]
    fn compositing_is_premultiplied() {
        let mut c = [0.0; 4];
        over(&mut c, [1.0, 0.5, 0.0], 0.5);
        assert_eq!(c, [0.5, 0.25, 0.0, 0.5]);
        over(&mut c, [0.0, 0.0, 0.0], 1.0);
        assert_eq!(c, [0.0, 0.0, 0.0, 1.0]);
    }
}
