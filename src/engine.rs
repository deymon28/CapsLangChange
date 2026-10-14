//! Binding engine: decides for every keyboard event whether it passes through or is swallowed,
//! and which binding (if any) must run.
//!
//! Pure code: no Win32 calls, fully unit-tested. The hook layer feeds it events and executes its decisions.

use std::sync::Arc;

use crate::config::{Action, AppInfo, Config, LookupFailure};
use crate::keys::{ModPattern, Shortcut, TAP, VK_LCONTROL, held, modifier_bit};

/// Scan code Windows reports for the synthetic Left Ctrl that accompanies AltGr (Right Alt).
const ALTGR_FAKE_CTRL_SCAN: u32 = 0x21D;

/// Longest press of a lone modifier that still counts as a tap, in milliseconds.
pub const TAP_MAX_MS: u32 = 500;

/// Origin of an event with respect to this program's own input injection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Physical keyboard or another program.
    External,
    /// Injected by us; always passes through.
    Ours,
    /// Injected by us to re-press a modifier the user was holding; dropped if the user has released it meanwhile.
    OursRestore,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Pass,
    Swallow {
        /// Inject a mask key so the swallowed chord's modifiers do not trigger Start menu, menu bar
        /// or the Windows language hotkey on release.
        mask: bool,
        /// Binding (index into `Config::bindings`) whose action must run.
        run: Option<usize>,
        /// Effective held modifiers at decision time.
        held: u8,
    },
    /// A lone modifier was tapped; the binding runs.
    Tap {
        /// Win or Alt: swallow the key-up and inject the mask key followed by the key-up, so the tap does
        /// not open the Start menu or activate a menu bar. Otherwise the key-up passes through.
        mask: bool,
        run: Option<usize>,
        held: u8,
    },
    /// The shortcut recorder captured a shortcut.
    Recorded {
        shortcut: Shortcut,
        /// Swallow this event: always for a recorded key-down; for a recorded Win or Alt tap, whose key-up is
        /// replaced as for Tap { mask: true, .. }.
        swallow: bool,
        mask: bool,
    },
}

pub struct Engine {
    config: Arc<Config>,
    /// Modifiers physically held, from the event stream (our own injections excluded).
    physical: u8,
    /// Keys whose key-down was swallowed; their auto-repeat and key-up are swallowed as well.
    swallowed: [bool; 256],
    /// Binding that fired for a swallowed key (used for auto-repeat).
    active: [Option<usize>; 256],
    /// Modifier pressed while no other modifier was held, and the time of that press: a tap candidate.
    tap: Option<(u8, u32)>,
    /// The next shortcut or modifier tap is recorded instead of processed.
    recording: bool,
}

impl Engine {
    pub fn new(config: Arc<Config>) -> Engine {
        Engine { config, physical: 0, swallowed: [false; 256], active: [None; 256], tap: None, recording: false }
    }

    pub fn set_recording(&mut self, on: bool) {
        self.recording = on;
    }

    /// Something other than the keyboard was used (for example a mouse click): no tap is pending.
    pub fn cancel_tap(&mut self) {
        self.tap = None;
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// Replaces the configuration. Keys currently swallowed keep being swallowed until released,
    /// so no key-up is ever delivered without its key-down.
    pub fn set_config(&mut self, config: Arc<Config>) {
        self.config = config;
        self.active = [None; 256];
    }

    #[cfg(test)]
    pub fn physical(&self) -> u8 {
        self.physical
    }

    pub fn set_physical(&mut self, bits: u8) {
        self.physical = bits;
    }

    /// Processes one event.
    ///
    /// `time` is the event time stamp in milliseconds. `held` returns the logical modifier state before
    /// this event; `foreground` identifies the active program. Both are evaluated lazily, only when a
    /// binding could match.
    #[allow(clippy::too_many_arguments)]
    pub fn on_key(
        &mut self,
        vk: u8,
        scan: u32,
        down: bool,
        time: u32,
        origin: Origin,
        held: impl FnOnce() -> u8,
        foreground: impl FnOnce() -> Option<AppInfo>,
    ) -> Decision {
        match origin {
            Origin::Ours => return Decision::Pass,
            Origin::OursRestore => {
                if down {
                    if let Some(bit) = modifier_bit(vk) {
                        if self.physical & bit == 0 {
                            return Decision::Swallow { mask: false, run: None, held: 0 };
                        }
                    }
                }
                return Decision::Pass;
            }
            Origin::External => {}
        }

        if let Some(bit) = modifier_bit(vk) {
            if vk == VK_LCONTROL && scan == ALTGR_FAKE_CTRL_SCAN {
                return Decision::Pass;
            }
            let others = self.physical & !bit;
            if down {
                self.physical |= bit;
                // Auto-repeat of the candidate keeps it; any other modifier press ends it.
                if self.tap.is_none_or(|(b, _)| b != bit) {
                    self.tap = (others == 0).then_some((bit, time));
                }
                return Decision::Pass;
            }
            self.physical &= !bit;
            return match self.tap.take() {
                Some((b, since)) if b == bit && time.wrapping_sub(since) <= TAP_MAX_MS => {
                    self.on_tap(bit, held, foreground)
                }
                _ => Decision::Pass,
            };
        }
        self.tap = None;

        let idx = vk as usize;
        if !down {
            if self.swallowed[idx] {
                self.swallowed[idx] = false;
                self.active[idx] = None;
                return Decision::Swallow { mask: false, run: None, held: 0 };
            }
            return Decision::Pass;
        }

        if self.recording {
            self.recording = false;
            let held_now = self.effective_held(held());
            self.swallowed[idx] = true;
            self.active[idx] = None;
            let shortcut = Shortcut { mods: ModPattern::from_held(held_now), vk };
            return Decision::Recorded { shortcut, swallow: true, mask: held_now != 0 };
        }

        if self.swallowed[idx] {
            // Auto-repeat of a swallowed key.
            let run = self.active[idx].filter(|&b| self.config.bindings.get(b).is_some_and(|b| b.repeat));
            let held_now = if run.is_some() { self.effective_held(held()) } else { 0 };
            return Decision::Swallow { mask: false, run, held: held_now };
        }

        let config = Arc::clone(&self.config);
        if !config.enabled || vk == TAP {
            return Decision::Pass;
        }
        let candidates = config.candidates(vk);
        if candidates.is_empty() {
            return Decision::Pass;
        }
        let held_now = self.effective_held(held());
        let matching = candidates.iter().copied().filter(|&b| config.bindings[b].shortcut.mods.matches(held_now));
        let Some(b) = pick(&config, matching, foreground) else {
            return Decision::Pass;
        };
        match config.bindings[b].action {
            Action::Passthrough => Decision::Pass,
            Action::Block => {
                self.swallowed[idx] = true;
                self.active[idx] = None;
                Decision::Swallow { mask: held_now != 0, run: None, held: held_now }
            }
            _ => {
                self.swallowed[idx] = true;
                self.active[idx] = Some(b);
                Decision::Swallow { mask: held_now != 0, run: Some(b), held: held_now }
            }
        }
    }

    /// A lone modifier (`bit`) was pressed and released.
    fn on_tap(&mut self, bit: u8, held: impl FnOnce() -> u8, foreground: impl FnOnce() -> Option<AppInfo>) -> Decision {
        let mask = bit & (held::WIN | held::ALT) != 0;
        if self.recording {
            self.recording = false;
            return Decision::Recorded {
                shortcut: Shortcut { mods: ModPattern::single(bit), vk: TAP },
                swallow: mask,
                mask,
            };
        }
        let config = Arc::clone(&self.config);
        if !config.enabled {
            return Decision::Pass;
        }
        let matching =
            config.candidates(TAP).iter().copied().filter(|&b| config.bindings[b].shortcut.mods.matches(bit));
        let Some(b) = pick(&config, matching, foreground) else {
            return Decision::Pass;
        };
        // The logical state still includes the tapped modifier. Actions lift it before injecting, unless the
        // hook releases it right away (mask).
        let mut held_now = self.effective_held(held());
        if mask {
            held_now &= !bit;
        }
        match config.bindings[b].action {
            Action::Passthrough => Decision::Pass,
            Action::Block => Decision::Tap { mask, run: None, held: held_now },
            _ => Decision::Tap { mask, run: Some(b), held: held_now },
        }
    }

    /// Removes the synthetic Left Ctrl that Windows reports while AltGr is held.
    fn effective_held(&self, logical: u8) -> u8 {
        if logical & held::RALT != 0 && self.physical & held::LCTRL == 0 { logical & !held::LCTRL } else { logical }
    }
}

/// The first of the matching bindings (in priority order) that applies to the foreground program.
/// The foreground program is identified only if there is a matching binding at all.
fn pick(
    config: &Config,
    mut matching: impl Iterator<Item = usize>,
    foreground: impl FnOnce() -> Option<AppInfo>,
) -> Option<usize> {
    let first = matching.next()?;
    let app = foreground();
    match &app {
        None if config.on_lookup_failure == LookupFailure::Pass => return None,
        Some(app) if config.is_excluded(app) => return None,
        _ => {}
    }
    std::iter::once(first).chain(matching).find(|&b| config.bindings[b].scope_allows(app.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use crate::keys::{VK_CAPITAL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_SPACE};

    const CTRL_ALT_SHIFT: u8 = held::LCTRL | held::LALT | held::LSHIFT;

    fn engine(text: &str) -> Engine {
        Engine::new(Arc::new(parse(text).unwrap()))
    }

    fn app(exe: &str) -> Option<AppInfo> {
        Some(AppInfo::from_path(&format!(r"C:\Apps\{exe}"), "Window"))
    }

    fn key(e: &mut Engine, vk: u8, down: bool, held: u8, fg: Option<AppInfo>) -> Decision {
        e.on_key(vk, 0, down, 0, Origin::External, || held, || fg)
    }

    fn run(b: usize, held: u8) -> Decision {
        Decision::Swallow { mask: held != 0, run: Some(b), held }
    }

    const SWALLOW: Decision = Decision::Swallow { mask: false, run: None, held: 0 };

    const BASIC: &str = r#"
[[binding]]
keys = "CapsLock"
action = "switch_layout"
[[binding]]
keys = "Ctrl+Alt+Shift+Space"
action = "capslock"
[[exclude]]
exe = "game.exe"
"#;

    #[test]
    fn caps_switches_and_up_is_swallowed() {
        let mut e = engine(BASIC);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("notepad.exe")), run(0, 0));
        // Auto-repeat: swallowed, no action.
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("notepad.exe")), SWALLOW);
        assert_eq!(key(&mut e, VK_CAPITAL, false, 0, app("notepad.exe")), SWALLOW);
        // Next press acts again.
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("notepad.exe")), run(0, 0));
    }

    #[test]
    fn excluded_program_gets_real_key() {
        let mut e = engine(BASIC);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("GAME.exe")), Decision::Pass);
        assert_eq!(key(&mut e, VK_CAPITAL, false, 0, app("GAME.exe")), Decision::Pass);
    }

    #[test]
    fn decision_sticks_when_foreground_changes_between_down_and_up() {
        let mut e = engine(BASIC);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("notepad.exe")), run(0, 0));
        assert_eq!(key(&mut e, VK_CAPITAL, false, 0, app("game.exe")), SWALLOW);

        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("game.exe")), Decision::Pass);
        assert_eq!(key(&mut e, VK_CAPITAL, false, 0, app("notepad.exe")), Decision::Pass);
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let mut e = engine(BASIC);
        // Shift+CapsLock is not bound: real Caps Lock.
        assert_eq!(key(&mut e, VK_CAPITAL, true, held::LSHIFT, app("a.exe")), Decision::Pass);
        assert_eq!(key(&mut e, VK_CAPITAL, false, held::LSHIFT, app("a.exe")), Decision::Pass);
        // Chord.
        assert_eq!(key(&mut e, VK_SPACE, true, CTRL_ALT_SHIFT, app("a.exe")), run(1, CTRL_ALT_SHIFT));
        assert_eq!(key(&mut e, VK_SPACE, false, CTRL_ALT_SHIFT, app("a.exe")), SWALLOW);
        // Plain space and partial chords pass.
        assert_eq!(key(&mut e, VK_SPACE, true, 0, app("a.exe")), Decision::Pass);
        assert_eq!(key(&mut e, VK_SPACE, true, held::LCTRL | held::LALT, app("a.exe")), Decision::Pass);
    }

    #[test]
    fn foreground_is_only_queried_for_candidates() {
        let mut e = engine(BASIC);
        let d = e.on_key(b'K', 0, true, 0, Origin::External, || 0, || panic!("foreground queried"));
        assert_eq!(d, Decision::Pass);
        let d = e.on_key(VK_SPACE, 0, true, 0, Origin::External, || 0, || panic!("foreground queried"));
        assert_eq!(d, Decision::Pass);
    }

    #[test]
    fn own_injections_pass() {
        let mut e = engine(BASIC);
        let d = e.on_key(VK_CAPITAL, 0, true, 0, Origin::Ours, || 0, || app("a.exe"));
        assert_eq!(d, Decision::Pass);
        let d = e.on_key(VK_CAPITAL, 0, false, 0, Origin::Ours, || 0, || app("a.exe"));
        assert_eq!(d, Decision::Pass);
    }

    #[test]
    fn restore_is_dropped_after_physical_release() {
        let mut e = engine(BASIC);
        key(&mut e, VK_LSHIFT, true, 0, None);
        assert_eq!(e.on_key(VK_LSHIFT, 0, true, 0, Origin::OursRestore, || 0, || None), Decision::Pass);
        key(&mut e, VK_LSHIFT, false, 0, None);
        assert_eq!(e.on_key(VK_LSHIFT, 0, true, 0, Origin::OursRestore, || 0, || None), SWALLOW);
        // Our own modifier events never change the physical table.
        e.on_key(VK_LWIN, 0, true, 0, Origin::Ours, || 0, || None);
        assert_eq!(e.physical(), 0);
    }

    #[test]
    fn disabled_config_passes_but_finishes_pending_keys() {
        let mut e = engine(BASIC);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("a.exe")), run(0, 0));
        e.set_config(Arc::new(parse("enabled = false\n[[binding]]\nkeys = \"CapsLock\"\naction = \"block\"").unwrap()));
        assert_eq!(key(&mut e, VK_CAPITAL, false, 0, app("a.exe")), SWALLOW);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("a.exe")), Decision::Pass);
    }

    #[test]
    fn scope_and_priority() {
        let mut e = engine(
            r#"
[[binding]]
keys = "F1"
action = "block"
[[binding]]
keys = "F1"
action = "capslock"
apps_only = ["editor.exe"]
[[binding]]
keys = "F2"
action = "passthrough"
apps_only = ["editor.exe"]
[[binding]]
keys = "F2"
action = "block"
[[binding]]
keys = "F3"
action = "block"
apps_except = ["editor.exe"]
"#,
        );
        assert_eq!(key(&mut e, 0x70, true, 0, app("editor.exe")), run(1, 0));
        key(&mut e, 0x70, false, 0, app("editor.exe"));
        assert_eq!(key(&mut e, 0x70, true, 0, app("other.exe")), SWALLOW);
        key(&mut e, 0x70, false, 0, app("other.exe"));
        assert_eq!(key(&mut e, 0x71, true, 0, app("editor.exe")), Decision::Pass);
        assert_eq!(key(&mut e, 0x71, true, 0, app("other.exe")), SWALLOW);
        assert_eq!(key(&mut e, 0x72, true, 0, app("editor.exe")), Decision::Pass);
    }

    #[test]
    fn lookup_failure_policy() {
        let mut e = engine(BASIC);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, None), run(0, 0));
        key(&mut e, VK_CAPITAL, false, 0, None);
        let mut e = engine(&format!("on_lookup_failure = \"pass\"\n{BASIC}"));
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, None), Decision::Pass);
    }

    #[test]
    fn repeat_bindings_run_again() {
        let mut e = engine("[[binding]]\nkeys = \"Ctrl+J\"\naction = \"send_keys\"\nsend = \"Down\"\nrepeat = true");
        assert_eq!(key(&mut e, b'J', true, held::LCTRL, app("a.exe")), run(0, held::LCTRL));
        assert_eq!(
            key(&mut e, b'J', true, held::LCTRL, app("a.exe")),
            Decision::Swallow { mask: false, run: Some(0), held: held::LCTRL }
        );
        assert_eq!(key(&mut e, b'J', false, held::LCTRL, app("a.exe")), SWALLOW);
    }

    #[test]
    fn altgr_fake_ctrl_is_ignored() {
        let mut e = engine("[[binding]]\nkeys = \"RAlt+Space\"\naction = \"block\"");
        // AltGr: synthetic LCtrl (scan 0x21D) followed by RAlt.
        e.on_key(VK_LCONTROL, ALTGR_FAKE_CTRL_SCAN, true, 0, Origin::External, || 0, || None);
        e.on_key(VK_RMENU, 0x38, true, 0, Origin::External, || held::LCTRL, || None);
        assert_eq!(e.physical(), held::RALT);
        let logical = held::LCTRL | held::RALT;
        assert_eq!(
            key(&mut e, VK_SPACE, true, logical, app("a.exe")),
            Decision::Swallow { mask: true, run: None, held: held::RALT }
        );
        // A real LCtrl press is kept.
        let mut e = engine("[[binding]]\nkeys = \"RAlt+Space\"\naction = \"block\"");
        key(&mut e, VK_LCONTROL, true, 0, None);
        key(&mut e, VK_RMENU, true, held::LCTRL, None);
        assert_eq!(key(&mut e, VK_SPACE, true, logical, app("a.exe")), Decision::Pass);
    }

    #[test]
    fn modifiers_are_never_swallowed() {
        let mut e = engine("[[binding]]\nkeys = \"Alt+X\"\naction = \"block\"");
        assert_eq!(key(&mut e, VK_LMENU, true, 0, app("a.exe")), Decision::Pass);
        assert_eq!(
            key(&mut e, b'X', true, held::LALT, app("a.exe")),
            Decision::Swallow { mask: true, run: None, held: held::LALT }
        );
        assert_eq!(key(&mut e, VK_LMENU, false, held::LALT, app("a.exe")), Decision::Pass);
        assert_eq!(key(&mut e, b'X', false, 0, app("a.exe")), SWALLOW);
    }

    #[test]
    fn recorder_captures_one_shortcut() {
        let mut e = engine(BASIC);
        e.set_recording(true);
        // Modifiers pass; the main key is recorded and swallowed together with its key-up.
        assert_eq!(key(&mut e, VK_LWIN, true, 0, None), Decision::Pass);
        let shortcut = Shortcut::parse("Win+R").unwrap();
        assert_eq!(
            key(&mut e, b'R', true, held::LWIN, None),
            Decision::Recorded { shortcut, swallow: true, mask: true }
        );
        assert_eq!(key(&mut e, b'R', false, held::LWIN, None), SWALLOW);
        assert_eq!(key(&mut e, b'R', false, 0, None), Decision::Pass);
        // Recording ends after one shortcut.
        key(&mut e, VK_LWIN, false, held::LWIN, None);
        assert_eq!(key(&mut e, VK_CAPITAL, true, 0, app("a.exe")), run(0, 0));
    }

    #[test]
    fn recorder_handles_altgr_and_taps() {
        let mut e = engine(BASIC);
        e.set_recording(true);
        e.on_key(VK_LCONTROL, ALTGR_FAKE_CTRL_SCAN, true, 0, Origin::External, || 0, || None);
        e.on_key(VK_RMENU, 0x38, true, 0, Origin::External, || held::LCTRL, || None);
        let d = key(&mut e, b'Q', true, held::LCTRL | held::RALT, None);
        assert_eq!(d, Decision::Recorded { shortcut: Shortcut::parse("Alt+Q").unwrap(), swallow: true, mask: true });

        let mut e = engine(BASIC);
        e.set_recording(true);
        e.on_key(VK_RCONTROL, 0x1D, true, 100, Origin::External, || 0, || None);
        let d = e.on_key(VK_RCONTROL, 0x1D, false, 200, Origin::External, || held::RCTRL, || None);
        assert_eq!(d, Decision::Recorded { shortcut: Shortcut::parse("RCtrl").unwrap(), swallow: false, mask: false });
    }

    fn tap(e: &mut Engine, vk: u8, down_at: u32, up_at: u32, fg: Option<AppInfo>) -> Decision {
        let bit = modifier_bit(vk).unwrap();
        let fg2 = fg.clone();
        assert_eq!(e.on_key(vk, 0, true, down_at, Origin::External, || 0, || fg), Decision::Pass);
        e.on_key(vk, 0, false, up_at, Origin::External, || bit, || fg2)
    }

    const TAPS: &str = r#"
[[binding]]
keys = "RCtrl"
action = "switch_layout"
[[binding]]
keys = "Alt"
action = "capslock"
[[binding]]
keys = "LWin"
action = "block"
apps_only = ["game.exe"]
"#;

    #[test]
    fn modifier_tap_fires_on_release() {
        let mut e = engine(TAPS);
        assert_eq!(
            tap(&mut e, VK_RCONTROL, 1000, 1100, app("a.exe")),
            Decision::Tap { mask: false, run: Some(0), held: held::RCTRL }
        );
        // Left Ctrl is not bound; either Alt is, and is masked.
        assert_eq!(tap(&mut e, VK_LCONTROL, 0, 50, app("a.exe")), Decision::Pass);
        assert_eq!(tap(&mut e, VK_RMENU, 0, 50, app("a.exe")), Decision::Tap { mask: true, run: Some(1), held: 0 });
        // Scoped block: mask only.
        assert_eq!(tap(&mut e, VK_LWIN, 0, 50, app("game.exe")), Decision::Tap { mask: true, run: None, held: 0 });
        assert_eq!(tap(&mut e, VK_LWIN, 0, 50, app("a.exe")), Decision::Pass);
    }

    #[test]
    fn modifier_tap_is_cancelled() {
        let mut e = engine(TAPS);
        // Held too long.
        assert_eq!(tap(&mut e, VK_RCONTROL, 0, TAP_MAX_MS + 1, app("a.exe")), Decision::Pass);
        // Used as a modifier.
        key(&mut e, VK_RCONTROL, true, 0, app("a.exe"));
        key(&mut e, b'C', true, held::RCTRL, app("a.exe"));
        key(&mut e, b'C', false, held::RCTRL, app("a.exe"));
        assert_eq!(key(&mut e, VK_RCONTROL, false, held::RCTRL, app("a.exe")), Decision::Pass);
        // Another modifier joined.
        key(&mut e, VK_RCONTROL, true, 0, app("a.exe"));
        key(&mut e, VK_LSHIFT, true, held::RCTRL, app("a.exe"));
        key(&mut e, VK_LSHIFT, false, held::RCTRL | held::LSHIFT, app("a.exe"));
        assert_eq!(key(&mut e, VK_RCONTROL, false, held::RCTRL, app("a.exe")), Decision::Pass);
        // Pressed while another modifier was held.
        key(&mut e, VK_LSHIFT, true, 0, app("a.exe"));
        assert_eq!(tap(&mut e, VK_RCONTROL, 0, 50, app("a.exe")), Decision::Pass);
        key(&mut e, VK_LSHIFT, false, held::LSHIFT, app("a.exe"));
        // Mouse click.
        key(&mut e, VK_RCONTROL, true, 0, app("a.exe"));
        e.cancel_tap();
        assert_eq!(key(&mut e, VK_RCONTROL, false, held::RCTRL, app("a.exe")), Decision::Pass);
        // Excluded program and paused configuration.
        let mut e = engine(&format!("{TAPS}\n[[exclude]]\nexe = \"game.exe\""));
        assert_eq!(tap(&mut e, VK_RCONTROL, 0, 50, app("game.exe")), Decision::Pass);
        let mut e = engine(&format!("enabled = false\n{TAPS}"));
        assert_eq!(tap(&mut e, VK_RCONTROL, 0, 50, app("a.exe")), Decision::Pass);
    }

    #[test]
    fn modifier_tap_survives_auto_repeat_and_altgr() {
        let mut e = engine(TAPS);
        e.on_key(VK_RCONTROL, 0, true, 0, Origin::External, || 0, || None);
        e.on_key(VK_RCONTROL, 0, true, 300, Origin::External, || held::RCTRL, || None);
        let d = e.on_key(VK_RCONTROL, 0, false, 400, Origin::External, || held::RCTRL, || app("a.exe"));
        assert_eq!(d, Decision::Tap { mask: false, run: Some(0), held: held::RCTRL });

        // AltGr: the synthetic Left Ctrl neither starts nor cancels a tap.
        e.on_key(VK_LCONTROL, ALTGR_FAKE_CTRL_SCAN, true, 0, Origin::External, || 0, || None);
        e.on_key(VK_RMENU, 0x38, true, 0, Origin::External, || held::LCTRL, || None);
        e.on_key(VK_LCONTROL, ALTGR_FAKE_CTRL_SCAN, false, 40, Origin::External, || held::LCTRL | held::RALT, || None);
        let d = e.on_key(VK_RMENU, 0x38, false, 50, Origin::External, || held::RALT, || app("a.exe"));
        assert_eq!(d, Decision::Tap { mask: true, run: Some(1), held: 0 });
        // A tap binding never reacts to an event with virtual-key code 0.
        assert_eq!(key(&mut e, 0, true, held::RCTRL, app("a.exe")), Decision::Pass);
    }
}
