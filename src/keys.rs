//! Key names, modifier patterns and shortcut text parsing/formatting.
//!
//! Pure code: no Win32 calls, fully unit-tested.

use std::fmt;

pub const VK_BACK: u8 = 0x08;
pub const VK_TAB: u8 = 0x09;
pub const VK_RETURN: u8 = 0x0D;
pub const VK_SHIFT: u8 = 0x10;
pub const VK_CONTROL: u8 = 0x11;
pub const VK_MENU: u8 = 0x12;
pub const VK_PAUSE: u8 = 0x13;
pub const VK_CAPITAL: u8 = 0x14;
pub const VK_ESCAPE: u8 = 0x1B;
pub const VK_SPACE: u8 = 0x20;
pub const VK_LWIN: u8 = 0x5B;
pub const VK_RWIN: u8 = 0x5C;
pub const VK_LSHIFT: u8 = 0xA0;
pub const VK_RSHIFT: u8 = 0xA1;
pub const VK_LCONTROL: u8 = 0xA2;
pub const VK_RCONTROL: u8 = 0xA3;
pub const VK_LMENU: u8 = 0xA4;
pub const VK_RMENU: u8 = 0xA5;
/// Unassigned virtual-key code used to "mask" swallowed modifier chords.
pub const VK_MASK: u8 = 0xE8;

/// Bit flags of held modifier keys, one bit per physical side.
pub mod held {
    pub const LCTRL: u8 = 1 << 0;
    pub const RCTRL: u8 = 1 << 1;
    pub const LALT: u8 = 1 << 2;
    pub const RALT: u8 = 1 << 3;
    pub const LSHIFT: u8 = 1 << 4;
    pub const RSHIFT: u8 = 1 << 5;
    pub const LWIN: u8 = 1 << 6;
    pub const RWIN: u8 = 1 << 7;
    pub const ALT: u8 = LALT | RALT;
    pub const WIN: u8 = LWIN | RWIN;
}

/// Side-specific virtual-key code for each `held` bit, in bit order.
pub const MODIFIER_VKS: [(u8, u8); 8] = [
    (held::LCTRL, VK_LCONTROL),
    (held::RCTRL, VK_RCONTROL),
    (held::LALT, VK_LMENU),
    (held::RALT, VK_RMENU),
    (held::LSHIFT, VK_LSHIFT),
    (held::RSHIFT, VK_RSHIFT),
    (held::LWIN, VK_LWIN),
    (held::RWIN, VK_RWIN),
];

/// Returns the `held` bit of a modifier virtual-key code. Generic codes map to the left side.
pub fn modifier_bit(vk: u8) -> Option<u8> {
    match vk {
        VK_LCONTROL | VK_CONTROL => Some(held::LCTRL),
        VK_RCONTROL => Some(held::RCTRL),
        VK_LMENU | VK_MENU => Some(held::LALT),
        VK_RMENU => Some(held::RALT),
        VK_LSHIFT | VK_SHIFT => Some(held::LSHIFT),
        VK_RSHIFT => Some(held::RSHIFT),
        VK_LWIN => Some(held::LWIN),
        VK_RWIN => Some(held::RWIN),
        _ => None,
    }
}

/// Which side(s) of a modifier a shortcut requires.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Side {
    /// The modifier must not be held.
    #[default]
    None,
    /// Either side (or both) must be held.
    Either,
    /// Only the left key must be held.
    Left,
    /// Only the right key must be held.
    Right,
}

impl Side {
    fn matches(self, left: bool, right: bool) -> bool {
        match self {
            Side::None => !left && !right,
            Side::Either => left || right,
            Side::Left => left && !right,
            Side::Right => right && !left,
        }
    }
}

/// Exact modifier requirements of a shortcut.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ModPattern {
    pub ctrl: Side,
    pub alt: Side,
    pub shift: Side,
    pub win: Side,
}

impl ModPattern {
    /// Exact match: every modifier not mentioned in the pattern must be released.
    pub fn matches(&self, held_bits: u8) -> bool {
        let h = |bit: u8| held_bits & bit != 0;
        self.ctrl.matches(h(held::LCTRL), h(held::RCTRL))
            && self.alt.matches(h(held::LALT), h(held::RALT))
            && self.shift.matches(h(held::LSHIFT), h(held::RSHIFT))
            && self.win.matches(h(held::LWIN), h(held::RWIN))
    }

    /// Number of side-specific modifiers; used to prefer more specific bindings.
    pub fn specificity(&self) -> u8 {
        [self.ctrl, self.alt, self.shift, self.win].iter().filter(|s| matches!(s, Side::Left | Side::Right)).count()
            as u8
    }

    /// Side-specific virtual-key codes to press for this pattern (`Either` presses the left key).
    pub fn press_vks(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut push = |side: Side, left: u8, right: u8| match side {
            Side::None => {}
            Side::Either | Side::Left => out.push(left),
            Side::Right => out.push(right),
        };
        push(self.ctrl, VK_LCONTROL, VK_RCONTROL);
        push(self.alt, VK_LMENU, VK_RMENU);
        push(self.shift, VK_LSHIFT, VK_RSHIFT);
        push(self.win, VK_LWIN, VK_RWIN);
        out
    }

    /// Pattern requiring exactly the modifier of one `held` bit, on that side.
    pub fn single(bit: u8) -> ModPattern {
        let side = |l: u8, r: u8| {
            if bit == l {
                Side::Left
            } else if bit == r {
                Side::Right
            } else {
                Side::None
            }
        };
        ModPattern {
            ctrl: side(held::LCTRL, held::RCTRL),
            alt: side(held::LALT, held::RALT),
            shift: side(held::LSHIFT, held::RSHIFT),
            win: side(held::LWIN, held::RWIN),
        }
    }

    /// Pattern describing exactly the given held bits, with generic sides where possible.
    pub fn from_held(held_bits: u8) -> ModPattern {
        let side = |l: u8, r: u8| match (held_bits & l != 0, held_bits & r != 0) {
            (false, false) => Side::None,
            _ => Side::Either,
        };
        ModPattern {
            ctrl: side(held::LCTRL, held::RCTRL),
            alt: side(held::LALT, held::RALT),
            shift: side(held::LSHIFT, held::RSHIFT),
            win: side(held::LWIN, held::RWIN),
        }
    }
}

/// A modifier pattern plus one main key, or a tap of a single modifier (`vk == TAP`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shortcut {
    pub mods: ModPattern,
    pub vk: u8,
}

/// `Shortcut::vk` of a modifier tap: the single modifier in `mods` is pressed and released alone.
pub const TAP: u8 = 0;

impl Shortcut {
    pub fn is_tap(&self) -> bool {
        self.vk == TAP
    }
}

/// Named keys. The first name listed for a code is the canonical one used when formatting.
const NAMED_KEYS: &[(&str, u8)] = &[
    ("CapsLock", VK_CAPITAL),
    ("Caps", VK_CAPITAL),
    ("Space", VK_SPACE),
    ("Tab", VK_TAB),
    ("Enter", VK_RETURN),
    ("Return", VK_RETURN),
    ("Esc", VK_ESCAPE),
    ("Escape", VK_ESCAPE),
    ("Backspace", VK_BACK),
    ("Insert", 0x2D),
    ("Ins", 0x2D),
    ("Delete", 0x2E),
    ("Del", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PgUp", 0x21),
    ("PageDown", 0x22),
    ("PgDn", 0x22),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("PrintScreen", 0x2C),
    ("PrtSc", 0x2C),
    ("ScrollLock", 0x91),
    ("Pause", VK_PAUSE),
    ("NumLock", 0x90),
    ("Menu", 0x5D),
    ("Apps", 0x5D),
    ("NumpadMultiply", 0x6A),
    ("NumpadAdd", 0x6B),
    ("NumpadSubtract", 0x6D),
    ("NumpadDecimal", 0x6E),
    ("NumpadDivide", 0x6F),
    ("Grave", 0xC0),
    ("Backtick", 0xC0),
    ("Minus", 0xBD),
    ("Equals", 0xBB),
    ("LeftBracket", 0xDB),
    ("RightBracket", 0xDD),
    ("Backslash", 0xDC),
    ("Semicolon", 0xBA),
    ("Quote", 0xDE),
    ("Comma", 0xBC),
    ("Period", 0xBE),
    ("Slash", 0xBF),
    ("IntlBackslash", 0xE2),
    ("VolumeMute", 0xAD),
    ("VolumeDown", 0xAE),
    ("VolumeUp", 0xAF),
    ("MediaNext", 0xB0),
    ("MediaPrevious", 0xB1),
    ("MediaStop", 0xB2),
    ("MediaPlayPause", 0xB3),
    ("BrowserBack", 0xA6),
    ("BrowserForward", 0xA7),
    ("BrowserRefresh", 0xA8),
    ("BrowserStop", 0xA9),
    ("BrowserSearch", 0xAA),
    ("BrowserFavorites", 0xAB),
    ("BrowserHome", 0xAC),
    ("LaunchMail", 0xB4),
    ("LaunchMedia", 0xB5),
    ("LaunchApp1", 0xB6),
    ("LaunchApp2", 0xB7),
    ("Clear", 0x0C),
    ("Help", 0x2F),
    ("Sleep", 0x5F),
    ("Kana", 0x15),
    ("Hangul", 0x15),
    ("Kanji", 0x19),
    ("Hanja", 0x19),
    ("Convert", 0x1C),
    ("NonConvert", 0x1D),
];

enum Part {
    Modifier { kind: usize, side: Side },
    Key(u8),
}

fn parse_part(raw: &str) -> Option<Part> {
    let lower = raw.to_ascii_lowercase();
    // kind: 0 ctrl, 1 alt, 2 shift, 3 win
    let modifier = match lower.as_str() {
        "ctrl" | "control" => Some((0, Side::Either)),
        "lctrl" => Some((0, Side::Left)),
        "rctrl" => Some((0, Side::Right)),
        "alt" => Some((1, Side::Either)),
        "lalt" => Some((1, Side::Left)),
        "ralt" | "altgr" => Some((1, Side::Right)),
        "shift" => Some((2, Side::Either)),
        "lshift" => Some((2, Side::Left)),
        "rshift" => Some((2, Side::Right)),
        "win" | "windows" => Some((3, Side::Either)),
        "lwin" => Some((3, Side::Left)),
        "rwin" => Some((3, Side::Right)),
        _ => None,
    };
    if let Some((kind, side)) = modifier {
        return Some(Part::Modifier { kind, side });
    }
    key_code(&lower).map(Part::Key)
}

/// Parses a non-modifier key name (case-insensitive).
pub fn key_code(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0];
        if c.is_ascii_lowercase() {
            return Some(c.to_ascii_uppercase());
        }
        if c.is_ascii_digit() {
            return Some(c);
        }
    }
    if let Some(n) = lower.strip_prefix("numpad") {
        if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() {
            return Some(0x60 + (n.as_bytes()[0] - b'0'));
        }
    }
    if let Some(n) = lower.strip_prefix('f') {
        if let Ok(n) = n.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }
    if let Some(hex) = lower.strip_prefix("vk:0x") {
        if let Ok(v) = u8::from_str_radix(hex, 16) {
            if v != 0 && modifier_bit(v).is_none() {
                return Some(v);
            }
        }
        return None;
    }
    NAMED_KEYS.iter().find(|(n, _)| n.eq_ignore_ascii_case(&lower)).map(|&(_, vk)| vk)
}

/// Canonical name of a non-modifier key code.
pub fn key_name(vk: u8) -> String {
    match vk {
        b'A'..=b'Z' | b'0'..=b'9' => (vk as char).to_string(),
        0x60..=0x69 => format!("Numpad{}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => NAMED_KEYS
            .iter()
            .find(|&&(_, v)| v == vk)
            .map(|&(n, _)| n.to_string())
            .unwrap_or_else(|| format!("vk:0x{vk:02X}")),
    }
}

impl Shortcut {
    /// Parses text such as `Ctrl+Alt+Shift+Space`, `CapsLock` or a lone modifier such as `RCtrl`.
    pub fn parse(text: &str) -> Result<Shortcut, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("empty shortcut".into());
        }
        let mut sides = [Side::None; 4];
        let mut key = None;
        let mut modifiers = 0;
        for raw in text.split('+') {
            let raw = raw.trim();
            if raw.is_empty() {
                return Err(format!("'{text}': empty part (use key names, e.g. Equals instead of '+')"));
            }
            match parse_part(raw) {
                Some(Part::Modifier { kind, side }) => {
                    if sides[kind] != Side::None {
                        return Err(format!("'{text}': modifier '{raw}' given twice"));
                    }
                    sides[kind] = side;
                    modifiers += 1;
                }
                Some(Part::Key(vk)) => {
                    if key.is_some() {
                        return Err(format!("'{text}': only one non-modifier key is allowed"));
                    }
                    key = Some(vk);
                }
                None => return Err(format!("'{text}': unknown key '{raw}'")),
            }
        }
        let mods = ModPattern { ctrl: sides[0], alt: sides[1], shift: sides[2], win: sides[3] };
        match key {
            Some(vk) => Ok(Shortcut { mods, vk }),
            None if modifiers == 1 => Ok(Shortcut { mods, vk: TAP }),
            None => Err(format!(
                "'{text}': a non-modifier key is required (a modifier alone works only as a single key, e.g. RCtrl)"
            )),
        }
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts =
            [(self.mods.ctrl, "Ctrl"), (self.mods.alt, "Alt"), (self.mods.shift, "Shift"), (self.mods.win, "Win")];
        let mut first = true;
        for (side, name) in parts {
            let prefix = match side {
                Side::None => continue,
                Side::Either => "",
                Side::Left => "L",
                Side::Right => "R",
            };
            if !first {
                f.write_str("+")?;
            }
            write!(f, "{prefix}{name}")?;
            first = false;
        }
        if self.is_tap() {
            return Ok(());
        }
        if !first {
            f.write_str("+")?;
        }
        f.write_str(&key_name(self.vk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_and_chords() {
        let s = Shortcut::parse("CapsLock").unwrap();
        assert_eq!(s.vk, VK_CAPITAL);
        assert_eq!(s.mods, ModPattern::default());

        let s = Shortcut::parse("ctrl + alt+SHIFT+space").unwrap();
        assert_eq!(s.vk, VK_SPACE);
        assert_eq!(s.mods.ctrl, Side::Either);
        assert_eq!(s.mods.alt, Side::Either);
        assert_eq!(s.mods.shift, Side::Either);
        assert_eq!(s.mods.win, Side::None);

        let s = Shortcut::parse("Win+Ctrl+Shift+Space").unwrap();
        assert_eq!(s.mods.win, Side::Either);

        let s = Shortcut::parse("RAlt+F13").unwrap();
        assert_eq!(s.mods.alt, Side::Right);
        assert_eq!(s.vk, 0x7C);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Shortcut::parse("").is_err());
        assert!(Shortcut::parse("Ctrl+Alt").is_err());
        assert!(Shortcut::parse("Ctrl+Spcae").is_err());
        assert!(Shortcut::parse("Ctrl++").is_err());
        assert!(Shortcut::parse("A+B").is_err());
        assert!(Shortcut::parse("Ctrl+LCtrl+A").is_err());
        assert!(Shortcut::parse("vk:0xA0").is_err());
        assert!(Shortcut::parse("F25").is_err());
    }

    #[test]
    fn key_names_round_trip() {
        for vk in 1..=255u8 {
            if modifier_bit(vk).is_some() {
                continue;
            }
            let name = key_name(vk);
            assert_eq!(key_code(&name), Some(vk), "{name}");
        }
        assert_eq!(key_code("caps"), Some(VK_CAPITAL));
        assert_eq!(key_code("7"), Some(b'7'));
        assert_eq!(key_code("numpad7"), Some(0x67));
        assert_eq!(key_code("f24"), Some(0x87));
    }

    #[test]
    fn display_is_canonical() {
        let s = Shortcut::parse("shift+ctrl+win+space").unwrap();
        assert_eq!(s.to_string(), "Ctrl+Shift+Win+Space");
        let s = Shortcut::parse("lctrl+caps").unwrap();
        assert_eq!(s.to_string(), "LCtrl+CapsLock");
        assert_eq!(Shortcut::parse(&s.to_string()).unwrap(), s);
    }

    #[test]
    fn exact_modifier_matching() {
        let caps = Shortcut::parse("CapsLock").unwrap().mods;
        assert!(caps.matches(0));
        assert!(!caps.matches(held::LSHIFT));

        let chord = Shortcut::parse("Ctrl+Alt+Shift+Space").unwrap().mods;
        assert!(chord.matches(held::LCTRL | held::LALT | held::LSHIFT));
        assert!(chord.matches(held::RCTRL | held::LALT | held::RSHIFT));
        assert!(!chord.matches(held::LCTRL | held::LALT));
        assert!(!chord.matches(held::LCTRL | held::LALT | held::LSHIFT | held::LWIN));

        let left = Shortcut::parse("LCtrl+A").unwrap().mods;
        assert!(left.matches(held::LCTRL));
        assert!(!left.matches(held::RCTRL));
        assert!(!left.matches(held::LCTRL | held::RCTRL));
        assert_eq!(left.specificity(), 1);
    }

    #[test]
    fn modifier_taps() {
        let s = Shortcut::parse("rctrl").unwrap();
        assert!(s.is_tap());
        assert_eq!(s.mods.ctrl, Side::Right);
        assert_eq!(s.to_string(), "RCtrl");
        assert!(s.mods.matches(held::RCTRL));
        assert!(!s.mods.matches(held::LCTRL));

        let s = Shortcut::parse("Win").unwrap();
        assert!(s.is_tap());
        assert_eq!(s.to_string(), "Win");
        assert!(s.mods.matches(held::LWIN) && s.mods.matches(held::RWIN));
        assert_eq!(Shortcut::parse("AltGr").unwrap().to_string(), "RAlt");
        assert!(!Shortcut::parse("Ctrl+A").unwrap().is_tap());

        assert_eq!(ModPattern::single(held::RSHIFT), Shortcut::parse("RShift").unwrap().mods);
        assert_eq!(ModPattern::single(held::LWIN), Shortcut::parse("LWin").unwrap().mods);
    }

    #[test]
    fn press_vks_and_from_held() {
        let s = Shortcut::parse("Win+RShift+Space").unwrap();
        assert_eq!(s.mods.press_vks(), vec![VK_RSHIFT, VK_LWIN]);
        let p = ModPattern::from_held(held::RCTRL | held::LALT);
        assert_eq!(p.ctrl, Side::Either);
        assert_eq!(p.alt, Side::Either);
        assert_eq!(p.shift, Side::None);
    }
}
