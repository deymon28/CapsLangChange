//! Synthetic keyboard input. Every event carries a tag in `dwExtraInfo` so the hook recognises its own input.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC,
    MapVirtualKeyW, SendInput,
};

use crate::keys::{MODIFIER_VKS, Shortcut, VK_CAPITAL, VK_MASK, VK_SPACE, held};
use crate::log;

pub const TAG_MASK: usize = 0xFFFF_FF00;
pub const TAG_BASE: usize = 0x434C_4300; // "CLC\0"
pub const TAG_NORMAL: usize = TAG_BASE;
pub const TAG_RESTORE: usize = TAG_BASE | 1;

fn is_extended(vk: u8) -> bool {
    matches!(
        vk,
        0x21..=0x28 // PageUp, PageDown, End, Home, arrows
            | 0x2C // PrintScreen
            | 0x2D | 0x2E // Insert, Delete
            | 0x5B | 0x5C | 0x5D // LWin, RWin, Apps
            | 0x6F // NumpadDivide
            | 0x90 // NumLock
            | 0xA3 | 0xA5 // RCtrl, RAlt
            | 0xA6..=0xB7 // browser and media keys
    )
}

fn key(vk: u8, up: bool, tag: usize) -> INPUT {
    // SAFETY: pure lookup.
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = if up { KEYEVENTF_KEYUP } else { 0 };
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk as u16, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: tag },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: the slice is valid for the call; cbSize is the size of one INPUT.
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        log::warning!(
            "inject",
            "SendInput sent {sent} of {} events (blocked by UIPI or another program?)",
            inputs.len()
        );
    }
}

fn push_mask(out: &mut Vec<INPUT>) {
    out.push(key(VK_MASK, false, TAG_NORMAL));
    out.push(key(VK_MASK, true, TAG_NORMAL));
}

/// Presses and releases the unassigned mask key, so released modifiers count as "used".
pub fn mask() {
    let mut v = Vec::with_capacity(2);
    push_mask(&mut v);
    send(&v);
}

/// Presses the mask key, then releases the modifier `vk`. Replaces the swallowed physical key-up of a lone
/// Win or Alt tap, so the mask is processed before the release (no Start menu, no menu bar).
pub fn mask_then_release(vk: u8) {
    let mut v = Vec::with_capacity(3);
    push_mask(&mut v);
    v.push(key(vk, true, TAG_NORMAL));
    send(&v);
}

/// Taps a shortcut while temporarily releasing the modifiers the user is holding (`held_bits`).
pub fn shortcut(sc: &Shortcut, held_bits: u8) {
    let mut v = Vec::with_capacity(20);
    let lifted: Vec<u8> = MODIFIER_VKS.iter().filter(|(bit, _)| held_bits & bit != 0).map(|&(_, vk)| vk).collect();
    for &vk in &lifted {
        v.push(key(vk, true, TAG_NORMAL));
    }
    let mods = sc.mods.press_vks();
    for &vk in &mods {
        v.push(key(vk, false, TAG_NORMAL));
    }
    if !sc.is_tap() {
        v.push(key(sc.vk, false, TAG_NORMAL));
        v.push(key(sc.vk, true, TAG_NORMAL));
    }
    for &vk in mods.iter().rev() {
        v.push(key(vk, true, TAG_NORMAL));
    }
    for &vk in &lifted {
        v.push(key(vk, false, TAG_RESTORE));
    }
    if held_bits & (held::WIN | held::ALT) != 0 {
        push_mask(&mut v);
    }
    send(&v);
}

/// Win+Space (or Win+Shift+Space for the previous layout).
pub fn win_space(previous: bool, held_bits: u8) {
    let text = if previous { "Win+Shift+Space" } else { "Win+Space" };
    let mut sc = Shortcut::parse(text).expect("static shortcut");
    sc.vk = VK_SPACE;
    shortcut(&sc, held_bits);
}

/// Taps the real Caps Lock key.
pub fn caps_tap() {
    send(&[key(VK_CAPITAL, false, TAG_NORMAL), key(VK_CAPITAL, true, TAG_NORMAL)]);
}

/// Current Caps Lock toggle state.
pub fn caps_on() -> bool {
    // SAFETY: plain query.
    unsafe { GetKeyState(VK_CAPITAL as i32) & 1 != 0 }
}
