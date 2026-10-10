//! Binding engine: decides for every keyboard event whether it passes through or is swallowed,
//! and which binding (if any) must run.
//!
//! Pure code: no Win32 calls, fully unit-tested. The hook layer feeds it events and executes its decisions.

use std::sync::Arc;

use crate::config::{Action, AppInfo, Config, LookupFailure};
use crate::keys::{VK_LCONTROL, held, modifier_bit};

/// Scan code Windows reports for the synthetic Left Ctrl that accompanies AltGr (Right Alt).
const ALTGR_FAKE_CTRL_SCAN: u32 = 0x21D;

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
}

pub struct Engine {
    config: Arc<Config>,
    /// Modifiers physically held, from the event stream (our own injections excluded).
    physical: u8,
    /// Keys whose key-down was swallowed; their auto-repeat and key-up are swallowed as well.
    swallowed: [bool; 256],
    /// Binding that fired for a swallowed key (used for auto-repeat).
    active: [Option<usize>; 256],
}

impl Engine {
    pub fn new(config: Arc<Config>) -> Engine {
        Engine { config, physical: 0, swallowed: [false; 256], active: [None; 256] }
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

    /// Marks a key as swallowed outside of binding processing (used by the shortcut recorder).
    pub fn mark_swallowed(&mut self, vk: u8) {
        self.swallowed[vk as usize] = true;
        self.active[vk as usize] = None;
    }

    /// Processes one event.
    ///
    /// `held` returns the logical modifier state before this event; `foreground` identifies the
    /// active program. Both are evaluated lazily, only when a binding could match.
    pub fn on_key(
        &mut self,
        vk: u8,
        scan: u32,
        down: bool,
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
            let fake_ctrl = vk == VK_LCONTROL && scan == ALTGR_FAKE_CTRL_SCAN;
            if !fake_ctrl {
                if down {
                    self.physical |= bit;
                } else {
                    self.physical &= !bit;
                }
            }
            return Decision::Pass;
        }

        let idx = vk as usize;
        if !down {
            if self.swallowed[idx] {
                self.swallowed[idx] = false;
                self.active[idx] = None;
                return Decision::Swallow { mask: false, run: None, held: 0 };
            }
            return Decision::Pass;
        }

        if self.swallowed[idx] {
            // Auto-repeat of a swallowed key.
            let run = self.active[idx].filter(|&b| self.config.bindings.get(b).is_some_and(|b| b.repeat));
            let held_now = if run.is_some() { self.effective_held(held()) } else { 0 };
            return Decision::Swallow { mask: false, run, held: held_now };
        }

        let config = Arc::clone(&self.config);
        if !config.enabled {
            return Decision::Pass;
        }
        let candidates = config.candidates(vk);
        if candidates.is_empty() {
            return Decision::Pass;
        }
        let held_now = self.effective_held(held());
        let mut matching = candidates.iter().copied().filter(|&b| config.bindings[b].shortcut.mods.matches(held_now));
        let Some(first) = matching.next() else {
            return Decision::Pass;
        };

        let app = foreground();
        match &app {
            None if config.on_lookup_failure == LookupFailure::Pass => return Decision::Pass,
            Some(app) if config.is_excluded(app) => return Decision::Pass,
            _ => {}
        }

        for b in std::iter::once(first).chain(matching) {
            let binding = &config.bindings[b];
            if !binding.scope_allows(app.as_ref()) {
                continue;
            }
            return match binding.action {
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
            };
        }
        Decision::Pass
    }

    /// Removes the synthetic Left Ctrl that Windows reports while AltGr is held.
    fn effective_held(&self, logical: u8) -> u8 {
        if logical & held::RALT != 0 && self.physical & held::LCTRL == 0 { logical & !held::LCTRL } else { logical }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use crate::keys::{VK_CAPITAL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RMENU, VK_SPACE};

    const CTRL_ALT_SHIFT: u8 = held::LCTRL | held::LALT | held::LSHIFT;

    fn engine(text: &str) -> Engine {
        Engine::new(Arc::new(parse(text).unwrap()))
    }

    fn app(exe: &str) -> Option<AppInfo> {
        Some(AppInfo::from_path(&format!(r"C:\Apps\{exe}"), "Window"))
    }

    fn key(e: &mut Engine, vk: u8, down: bool, held: u8, fg: Option<AppInfo>) -> Decision {
        e.on_key(vk, 0, down, Origin::External, || held, || fg)
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
        let d = e.on_key(b'K', 0, true, Origin::External, || 0, || panic!("foreground queried"));
        assert_eq!(d, Decision::Pass);
        let d = e.on_key(VK_SPACE, 0, true, Origin::External, || 0, || panic!("foreground queried"));
        assert_eq!(d, Decision::Pass);
    }

    #[test]
    fn own_injections_pass() {
        let mut e = engine(BASIC);
        let d = e.on_key(VK_CAPITAL, 0, true, Origin::Ours, || 0, || app("a.exe"));
        assert_eq!(d, Decision::Pass);
        let d = e.on_key(VK_CAPITAL, 0, false, Origin::Ours, || 0, || app("a.exe"));
        assert_eq!(d, Decision::Pass);
    }

    #[test]
    fn restore_is_dropped_after_physical_release() {
        let mut e = engine(BASIC);
        key(&mut e, VK_LSHIFT, true, 0, None);
        assert_eq!(e.on_key(VK_LSHIFT, 0, true, Origin::OursRestore, || 0, || None), Decision::Pass);
        key(&mut e, VK_LSHIFT, false, 0, None);
        assert_eq!(e.on_key(VK_LSHIFT, 0, true, Origin::OursRestore, || 0, || None), SWALLOW);
        // Our own modifier events never change the physical table.
        e.on_key(VK_LWIN, 0, true, Origin::Ours, || 0, || None);
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
        e.on_key(VK_LCONTROL, ALTGR_FAKE_CTRL_SCAN, true, Origin::External, || 0, || None);
        e.on_key(VK_RMENU, 0x38, true, Origin::External, || held::LCTRL, || None);
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
    fn recorder_marking() {
        let mut e = engine(BASIC);
        e.mark_swallowed(b'R');
        assert_eq!(key(&mut e, b'R', false, 0, None), SWALLOW);
        assert_eq!(key(&mut e, b'R', false, 0, None), Decision::Pass);
    }
}
