# Changelog

## Unreleased

- Language indicator: after a binding switches the layout, a small capsule next to the text cursor (or the mouse
  pointer) shows the new language, for example `EN` or `RU`. It stays while the key is held and fades out shortly
  after. Settings `indicator`, `indicator_position`, `indicator_duration_ms`, `indicator_height` and
  `indicator_labels`; tray menu item **Show the language after switching**.
- Settings added by a new version are written into an existing configuration file (with their comments) on start.

## 0.2.0

- Modifier taps as triggers: `keys = "RCtrl"` (or `RAlt`, `LShift`, `Win`, ...) fires when the modifier is pressed and
  released on its own. A lone Win or Alt tap bound this way does not open the Start menu or a menu bar.
- The shortcut recorder also records modifier taps, no longer opens the Start menu or a menu bar after recording a Win
  or Alt chord, and records AltGr chords as `Alt+...` instead of `Ctrl+Alt+...`.
- `layout = "..."` without `mode` now switches to that layout (it was silently ignored).
- New warnings: settings that have no effect for the action, unknown keys in inline program entries, layout ids that
  are not installed.
- More key names: browser and launch keys, `Clear`, `Help`, `Sleep` and IME keys.
- The tray keeps the excluded-program state while its own menu or dialogs are open.
- The hook is re-installed when returning to the session through fast user switching or Remote Desktop.
- The warning about an executable inside the user profile no longer matches other profiles with the same prefix.
- Release notes contain only the section of the released version.

## 0.1.0

First release.

- Caps Lock switches the keyboard layout (Win+Space or Windows API message).
- User-defined bindings: switch to next, previous or a specific layout, restricted layout cycles, real Caps Lock
  (toggle, on, off), send another shortcut, block, pass through.
- Per-binding program scope and a global list of excluded programs.
- Caps Lock state restored after leaving an excluded program.
- Tray menu: enable/pause, bindings, layout method, exclusions (including capture of the active program),
  shortcut recorder, autostart (normal or as administrator), config tools, installed layouts.
- Commented configuration file with validation and automatic reload.
