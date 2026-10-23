//! Keyboard layout switching through the Windows API (the "message" method) and layout listing.

use std::cell::RefCell;
use std::collections::HashMap;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows_sys::Win32::Globalization::LCIDToLocaleName;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardLayoutList, HKL};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, PostMessageW,
    WM_INPUTLANGCHANGEREQUEST,
};

use crate::config::{self, Config, Indicator, LayoutMode};
use crate::log;
use crate::util::{from_wide, wide};

const LAYOUTS_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Keyboard Layouts";

thread_local! {
    static KLID_CACHE: RefCell<HashMap<usize, u32>> = RefCell::new(HashMap::new());
}

/// Installed layouts (input locale handles) in Windows order.
pub fn installed() -> Vec<HKL> {
    // SAFETY: first call queries the count, second fills a buffer of that size.
    unsafe {
        let n = GetKeyboardLayoutList(0, std::ptr::null_mut());
        if n <= 0 {
            return Vec::new();
        }
        let mut list = vec![std::ptr::null_mut(); n as usize];
        let got = GetKeyboardLayoutList(n, list.as_mut_ptr());
        list.truncate(got.max(0) as usize);
        list
    }
}

/// KLIDs of the installed layouts, in Windows order.
pub fn installed_klids() -> Vec<u32> {
    installed().into_iter().map(klid_of).collect()
}

/// Parses configuration text and warns about layout ids that are not installed.
pub fn parse_config(text: &str) -> Result<Config, Vec<String>> {
    let mut config = config::parse(text)?;
    config.check_layouts(&installed_klids());
    Ok(config)
}

/// Resolves the KLIDs of all installed layouts now, so the first switch inside the keyboard hook
/// does not have to read the registry.
pub fn warm_cache() {
    for hkl in installed() {
        klid_of(hkl);
    }
}

/// Keyboard layout identifier (KLID) of an input locale handle, e.g. 0x00000419.
pub fn klid_of(hkl: HKL) -> u32 {
    let key = hkl as usize;
    if let Some(k) = KLID_CACHE.with(|c| c.borrow().get(&key).copied()) {
        return k;
    }
    let value = hkl as usize as u32;
    let device = (value >> 16) as u16;
    let klid = match device & 0xF000 {
        0xF000 => layout_id_to_klid(device & 0x0FFF).unwrap_or(value & 0xFFFF),
        0xE000 => value, // IME: the handle is the KLID
        _ => device as u32,
    };
    KLID_CACHE.with(|c| c.borrow_mut().insert(key, klid));
    klid
}

/// Finds the KLID whose registry "Layout Id" equals `id` (layout variants such as US-International).
fn layout_id_to_klid(id: u16) -> Option<u32> {
    // SAFETY: registry handles are checked and closed; buffers are sized and lengths passed.
    unsafe {
        let mut root: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(LAYOUTS_KEY).as_ptr(), 0, KEY_READ, &mut root) != ERROR_SUCCESS {
            return None;
        }
        let mut result = None;
        let mut index = 0;
        loop {
            let mut name = [0u16; 64];
            let mut len = name.len() as u32;
            let rc = RegEnumKeyExW(
                root,
                index,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if rc != ERROR_SUCCESS {
                break;
            }
            index += 1;
            let mut value = [0u16; 16];
            let mut size = (value.len() * 2) as u32;
            let rc = RegGetValueW(
                root,
                name.as_ptr(),
                wide("Layout Id").as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &mut size,
            );
            if rc == ERROR_SUCCESS && u16::from_str_radix(&from_wide(&value), 16).ok() == Some(id) {
                result = u32::from_str_radix(&from_wide(&name), 16).ok();
                break;
            }
        }
        RegCloseKey(root);
        result
    }
}

/// Registry display name of a layout ("Russian", "US", ...).
fn layout_text(klid: u32) -> Option<String> {
    let mut value = [0u16; 128];
    let mut size = (value.len() * 2) as u32;
    let sub = wide(format!(r"{LAYOUTS_KEY}\{klid:08X}"));
    // SAFETY: buffer size passed in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub.as_ptr(),
            wide("Layout Text").as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (rc == ERROR_SUCCESS).then(|| from_wide(&value))
}

fn locale_name(lang: u16) -> String {
    let mut buf = [0u16; 85];
    // SAFETY: buffer length passed.
    let n = unsafe { LCIDToLocaleName(lang as u32, buf.as_mut_ptr(), buf.len() as i32, 0) };
    if n > 0 { from_wide(&buf) } else { format!("{lang:04X}") }
}

/// Keyboard layout of the foreground window's thread.
pub fn foreground_layout() -> HKL {
    // SAFETY: plain queries.
    unsafe {
        let foreground = GetForegroundWindow();
        let thread = if foreground.is_null() { 0 } else { GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
        GetKeyboardLayout(thread)
    }
}

/// Short text for a layout: its own label from the indicator settings, otherwise the language code ("EN", "RU", "UK").
pub fn short_name(hkl: HKL, indicator: &Indicator) -> String {
    if let Some(label) = indicator.label(klid_of(hkl)) {
        return label.to_string();
    }
    let mut buf = [0u16; 85];
    let lang = hkl as usize as u32 & 0xFFFF;
    // SAFETY: buffer length passed.
    let n = unsafe { LCIDToLocaleName(lang, buf.as_mut_ptr(), buf.len() as i32, 0) };
    if n > 0 { language_code(&from_wide(&buf)) } else { "?".into() }
}

/// Upper-case language part of a locale name: "en-US" -> "EN", "sr-Latn-RS" -> "SR".
fn language_code(locale: &str) -> String {
    match locale.split('-').next() {
        Some(lang) if !lang.is_empty() => lang.to_uppercase(),
        _ => "?".into(),
    }
}

/// Human-readable list of installed layouts for the tray "Installed layouts" dialog.
pub fn describe_installed() -> Vec<String> {
    installed()
        .into_iter()
        .map(|hkl| {
            let klid = klid_of(hkl);
            let lang = (hkl as usize & 0xFFFF) as u16;
            let text = layout_text(klid).unwrap_or_else(|| "unknown layout".into());
            format!("{klid:08X}   {text}   ({})", locale_name(lang))
        })
        .collect()
}

/// Switches the layout of the foreground window by posting `WM_INPUTLANGCHANGEREQUEST` to its focus window.
pub fn switch_by_message(mode: LayoutMode, cycle: &[u32]) {
    // SAFETY: plain queries and a posted message; all handles come from the system.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_null() {
            return;
        }
        let thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        let target: HWND = if GetGUIThreadInfo(thread, &mut info) != 0 && !info.hwndFocus.is_null() {
            info.hwndFocus
        } else {
            foreground
        };
        let current = GetKeyboardLayout(thread);
        let all = installed();
        let Some(next) = choose(mode, cycle, current, &all) else {
            log::warning!("layout", "no matching installed layout for {mode:?} (cycle {cycle:08X?})");
            return;
        };
        if next != current && PostMessageW(target, WM_INPUTLANGCHANGEREQUEST, 0, next as isize) == 0 {
            log::warning!("layout", "could not post the layout change request to the active window");
        }
    }
}

fn choose(mode: LayoutMode, cycle: &[u32], current: HKL, all: &[HKL]) -> Option<HKL> {
    let pick = |klid: u32| all.iter().copied().find(|&h| klid_of(h) == klid);
    if let LayoutMode::Set(klid) = mode {
        return pick(klid);
    }
    let ring: Vec<HKL> = if cycle.is_empty() { all.to_vec() } else { cycle.iter().filter_map(|&k| pick(k)).collect() };
    if ring.is_empty() {
        return None;
    }
    let pos = ring.iter().position(|&h| h == current).or_else(|| {
        // Same layout loaded under a different handle (e.g. per-language variant): compare KLIDs.
        let k = klid_of(current);
        ring.iter().position(|&h| klid_of(h) == k)
    });
    let n = ring.len();
    Some(match (mode, pos) {
        (LayoutMode::Previous, Some(p)) => ring[(p + n - 1) % n],
        (_, Some(p)) => ring[(p + 1) % n],
        (_, None) => ring[0],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes() {
        assert_eq!(language_code("en-US"), "EN");
        assert_eq!(language_code("uk-UA"), "UK");
        assert_eq!(language_code("sr-Latn-RS"), "SR");
        assert_eq!(language_code("fil"), "FIL");
        assert_eq!(language_code(""), "?");
    }
}
