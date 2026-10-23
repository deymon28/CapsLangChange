# CapsLangChange

A tiny Windows tray utility that turns **Caps Lock into a keyboard layout switch**, and lets you bind
**any keyboard shortcut to an action**, with **per-program exceptions**.

It was written as a companion to (or replacement for) a PowerToys Keyboard Manager remap like
`CapsLock -> Win+Space`. Keyboard Manager applies such a remap everywhere, which breaks games and
programs that need a real Caps Lock. CapsLangChange does the same job, but in the programs you exclude,
every key reaches the program unchanged.

- Caps Lock switches the layout (by pressing Win+Space for you, or through the Windows API).
- Your own shortcuts: switch to the next, previous or a specific layout, cycle through chosen layouts,
  toggle or set the real Caps Lock, remap a shortcut to another one, or block it.
- Every shortcut can be limited to, or kept away from, specific programs.
- After a switch, the new language (`EN`, `RU`, ...) appears for a moment in a small box next to the text cursor.
- Excluded programs (for example games) get all keys untouched. Caps Lock is set back to how it was
  when you leave them, so you do not return to the desktop typing in capitals.
- Settings from the tray icon; everything else in one commented text file that is applied as you save it.
- One small executable (about 0.6 MB), no runtime, no installer, about 2 MB of private memory, no network access.

Windows 10 (1809 or later) and Windows 11, x64 and ARM64.

## Install

1. Download `CapsLangChange-<version>-x64.zip` (or `arm64`) from the
   [Releases](https://github.com/deymon28/CapsLangChange/releases) page and check it against `SHA256SUMS.txt`
   if you like.
2. Put `CapsLangChange.exe` in a folder of your choice. If you plan to use the administrator autostart (see below),
   use a folder normal programs cannot write to, such as `C:\Program Files\CapsLangChange`.
3. Run it. A tray icon appears and Caps Lock now switches the layout.
4. Right-click the tray icon > **Start with Windows** > **Normal** or **As administrator**.

The binary is not code-signed, so Windows SmartScreen may warn on first start ("More info" > "Run anyway").

### Coming from PowerToys Keyboard Manager

Remove the `CapsLock -> Win+Space` remap in Keyboard Manager (or turn Keyboard Manager off if you use it
for nothing else). Otherwise both programs react to Caps Lock. CapsLangChange shows a one-time notice if
Keyboard Manager is running. It never reads or changes PowerToys files.

If you also had shortcuts that produce a real Caps Lock (for example `Ctrl+Alt+Shift+Space -> CapsLock`),
add them as `capslock` bindings (see the examples below). Without any binding, `Shift+CapsLock` is already a
real Caps Lock, because a binding for plain `CapsLock` only fires when no modifier is held.

## Tray menu

| Item | What it does |
| --- | --- |
| Enabled | Pause or resume all bindings. |
| Bindings | Turn single bindings on or off. |
| Layout switch method | Default method for switching layouts: Win+Space, or a Windows API message. |
| Excluded programs | Remove an exclusion; add a running program; or add whatever program is active 5 seconds later (switch to a fullscreen game and wait). |
| Record a shortcut... | Press any combination; its text for the config file (for example `Win+Ctrl+Shift+Space`) is copied to the clipboard. |
| Restore Caps Lock after excluded programs | See [Caps Lock in excluded programs](#caps-lock-in-excluded-programs). |
| Show the language after switching | Turn the [language indicator](#language-indicator) on or off. |
| Start with Windows | Off, Normal, or As administrator. |
| Open config file / Reload config / Check config | Edit and validate the configuration. |
| Installed layouts | Shows the layout ids (`00000409` ...) to use in the config. |
| Open logs folder / About / Exit | |

The icon is blue when active, grey when paused, and amber while an excluded program is in the foreground.

## Configuration

The file is `%APPDATA%\CapsLangChange\config.toml`. It is created with comments on first start. Changes are applied
as soon as you save. If the file has an error, the previous settings stay active and a notification shows the
problem. **Check config** in the tray lists all errors.

### General settings

```toml
enabled = true                    # false = pause everything
method = "win_space"              # default layout switch method: "win_space" or "message"
restore_capslock_on_leave = true  # see "Caps Lock in excluded programs"
on_lookup_failure = "act"         # active program unknown: "act" = treat as normal, "pass" = do nothing
indicator = true                  # see "Language indicator"
```

### Bindings

Each `[[binding]]` section maps a shortcut to an action:

```toml
[[binding]]
name = "Switch layout"   # optional, shown in the tray menu
keys = "CapsLock"        # the shortcut
action = "switch_layout" # what it does
```

| Key | Meaning |
| --- | --- |
| `keys` | Modifiers and one key joined with `+`, for example `CapsLock`, `Ctrl+Alt+Shift+Space`, `RAlt+F1`; or a single modifier such as `RCtrl` (see [Modifier taps](#modifier-taps)). |
| `action` | `switch_layout`, `capslock`, `send_keys`, `block` or `passthrough` (see below). |
| `name` | Optional label. |
| `enabled` | `false` turns the binding off (also from the tray). |
| `apps_only` | Fire only in these programs, for example `["notepad.exe"]`. |
| `apps_except` | Never fire in these programs. |
| `repeat` | `true` repeats the action while the key is held (useful for `send_keys`). |

Modifiers must match exactly: `CapsLock` does not fire while Shift is held, and `Ctrl+J` does not fire for
`Ctrl+Shift+J`. If several bindings match, one limited to the current program (`apps_only`) wins over a general one,
then side-specific modifiers (`LCtrl`) win over generic ones (`Ctrl`), then the earlier binding in the file wins.

#### Actions

| `action` | Extra keys | Effect |
| --- | --- | --- |
| `switch_layout` | `mode` = `"next"` (default), `"previous"` or `"set"`; `layout` = layout id for `set` (`layout` alone implies `set`); `cycle` = list of layout ids; `method` = `"win_space"` or `"message"` | Changes the keyboard layout. |
| `capslock` | `state` = `"toggle"` (default), `"on"` or `"off"` | Changes the real Caps Lock. |
| `send_keys` | `send` = shortcut, for example `"Ctrl+C"` | Presses another shortcut instead. |
| `block` | | Swallows the shortcut. |
| `passthrough` | | Lets the shortcut through untouched (useful together with `apps_only` to make an exception). |

Layout switch methods:

- `win_space` presses Win+Space (Win+Shift+Space for `previous`) exactly like Windows does. It shows the Windows
  language popup and cycles through all installed layouts.
- `message` asks the active window directly through the Windows API (`WM_INPUTLANGCHANGEREQUEST`), without any
  synthetic keys. It is required for `mode = "set"` and `cycle`, which therefore always use this method. A few
  console and Store app windows may ignore it; use `win_space` there.

Layout ids are shown under tray menu > **Installed layouts**: for example `00000409` (English, US),
`00000419` (Russian), `00000422` (Ukrainian).

#### Key names

| Kind | Names |
| --- | --- |
| Modifiers | `Ctrl`, `Alt`, `Shift`, `Win` (either side); `LCtrl`, `RCtrl`, `LAlt`, `RAlt` (also `AltGr`), `LShift`, `RShift`, `LWin`, `RWin` |
| Letters and digits | `A`-`Z`, `0`-`9` |
| Function keys | `F1`-`F24` |
| Editing and navigation | `Space`, `Tab`, `Enter`, `Esc`, `Backspace`, `CapsLock`, `Insert`, `Delete`, `Home`, `End`, `PageUp`, `PageDown`, `Left`, `Up`, `Right`, `Down`, `PrintScreen`, `ScrollLock`, `Pause`, `NumLock`, `Menu` |
| Numpad | `Numpad0`-`Numpad9`, `NumpadMultiply`, `NumpadAdd`, `NumpadSubtract`, `NumpadDecimal`, `NumpadDivide` |
| Punctuation (US positions) | `Grave`, `Minus`, `Equals`, `LeftBracket`, `RightBracket`, `Backslash`, `Semicolon`, `Quote`, `Comma`, `Period`, `Slash`, `IntlBackslash` |
| Media | `VolumeMute`, `VolumeDown`, `VolumeUp`, `MediaNext`, `MediaPrevious`, `MediaStop`, `MediaPlayPause` |
| Browser and launch | `BrowserBack`, `BrowserForward`, `BrowserRefresh`, `BrowserStop`, `BrowserSearch`, `BrowserFavorites`, `BrowserHome`, `LaunchMail`, `LaunchMedia`, `LaunchApp1`, `LaunchApp2` |
| Other | `Clear`, `Help`, `Sleep`, `Kana` / `Hangul`, `Kanji` / `Hanja`, `Convert`, `NonConvert` |
| Anything else | `vk:0xNN` with the Windows virtual-key code |

**Record a shortcut** in the tray produces the right text for you, also for a modifier tap.

#### Modifier taps

A single modifier as `keys` (`RCtrl`, `RAlt`, `LShift`, `Win`, ...) fires when that key is pressed and released
on its own, within half a second, with no other key or mouse button used meanwhile. The key itself still reaches
Windows normally, so it keeps working as a modifier in shortcuts.

```toml
[[binding]]
name = "Right Ctrl switches the layout"
keys = "RCtrl"
action = "switch_layout"
```

A lone Win or Alt tap that triggers a binding does not open the Start menu or a menu bar. This also makes
`keys = "Win"` with `action = "block"` a way to stop the Start menu from opening on a lone Win press (for example only
in some programs, with `apps_only`). While a tap binding exists, CapsLangChange also watches mouse clicks (only to
cancel a pending tap; nothing is recorded).

### Language indicator

When a `switch_layout` binding fires, a small capsule next to the text cursor shows the language you switched to, as
a two-letter code (`EN`, `RU`, `UK`, `DE`, ...). It stays while the key is held (holding Caps Lock does not switch
again, so it simply shows the current language), and disappears `indicator_duration_ms` after the switch or after
you release the key. It never takes the focus and lets mouse clicks through. It follows the Windows light or dark
theme.

```toml
indicator = true                  # false = never show it (also from the tray)
indicator_position = "caret"      # "caret" = next to the text cursor, "mouse" = next to the mouse pointer
indicator_duration_ms = 500       # 100 to 10000
indicator_height = 15             # box height in pixels at 100% display scale, 10 to 60 (text scales with it)
indicator_labels = { "00000422" = "UA", "00010409" = "DV" }   # own text (up to 4 characters) per layout id
```

A configuration file from an older version gets the new settings with their comments the next time the program
starts; your other settings and comments stay as they are.

The text cursor is found through the standard Windows caret, or through accessibility for browsers, Electron apps and
other programs that draw their own. When a program reports neither, the capsule appears next to the mouse pointer.
The code comes from the layout's language, so two layouts of one language show the same code unless you give them
labels.

### Excluded programs

In an excluded program all bindings are suspended and every key reaches the program unchanged:

```toml
[[exclude]]
exe = "SomeGame.exe"          # executable name, not case-sensitive

[[exclude]]
path = "D:\\Games\\"          # every program under this folder

[[exclude]]
exe = "javaw.exe"
class = "GLFW30"              # only windows of this class
```

The tray menu adds exclusions for you. Use **Add the program active in 5 seconds** for fullscreen games, whose
executable name is often not obvious.

### Caps Lock in excluded programs

When you press Caps Lock in an excluded program (for example as a game key), Windows also turns the Caps Lock state on.
With `restore_capslock_on_leave = true`, CapsLangChange remembers the state when the excluded program becomes active,
and sets it back when you switch away.

### Example: Caps Lock layout switch plus real Caps Lock shortcuts and a game

```toml
method = "win_space"

[[binding]]
name = "Switch layout"
keys = "CapsLock"
action = "switch_layout"

[[binding]]
name = "Real Caps Lock"
keys = "Ctrl+Alt+Shift+Space"
action = "capslock"

[[binding]]
name = "Real Caps Lock (Win)"
keys = "Win+Ctrl+Shift+Space"
action = "capslock"

[[binding]]
name = "English"
keys = "Win+Ctrl+Shift+1"
action = "switch_layout"
mode = "set"
layout = "00000409"

[[binding]]
name = "Toggle English / Russian only"
keys = "RAlt+Space"
action = "switch_layout"
cycle = ["00000409", "00000419"]

[[exclude]]
exe = "SomeGame.exe"
```

## Administrator windows

Windows does not deliver keyboard events from windows running as administrator to normal programs. In such windows
CapsLangChange (like any normal program) cannot act, and Caps Lock behaves like a plain Caps Lock there.

Choose **Start with Windows > As administrator** to fix this. CapsLangChange asks for administrator rights once and
creates a scheduled task that starts it with administrator rights at every logon, without further prompts. Because a
program with administrator rights should not be replaceable by normal programs, keep the executable in a protected
folder such as `C:\Program Files\CapsLangChange`. CapsLangChange warns you if it lives inside your user profile.
Switching back to Off or Normal removes the task (with one more prompt if needed).

## Games and anticheat

CapsLangChange never touches game processes: it does not inject code or read their memory. In an excluded program it
also does not send any input. It does use a standard global keyboard hook (the same mechanism as PowerToys Keyboard
Manager and AutoHotkey). Most anticheat systems accept that, but no tool of this kind can guarantee it for every game.
If a game complains, quit CapsLangChange while playing it.

## Command line

```text
CapsLangChange.exe [--config <path>] [--debug]
CapsLangChange.exe --check-config [--config <path>]   exit code 0 = valid
CapsLangChange.exe --version | --help
```

From PowerShell, pipe the output to see it before the prompt returns, for example
`.\CapsLangChange.exe --check-config | Out-Host`.

## Troubleshooting

- **Caps Lock switches twice or not at all**: another remapper (PowerToys Keyboard Manager, AutoHotkey) also handles
  Caps Lock. Remove its remap.
- **Nothing happens in one particular window**: the window probably runs as administrator; see
  [Administrator windows](#administrator-windows).
- **A setting does not apply**: tray > **Check config**. The log is in `%LOCALAPPDATA%\CapsLangChange\logs`. Start with
  `--debug` to log which binding fired. Typed text is never logged.
- **The tray icon is hidden**: Windows settings > Personalization > Taskbar > Other system tray icons.
- **The language indicator shows the previous language or appears next to the mouse pointer**: some programs (a few
  Store apps and consoles) neither apply the switch to their main window thread nor report a text cursor. Use
  `indicator_position = "mouse"`, or turn it off with `indicator = false`.

## Privacy and security

- The keyboard hook looks at key codes only to match your shortcuts. Nothing you type is stored, logged or sent anywhere.
  The program has no network access.
- The configuration contains only shortcut text and program names.
- Release binaries are built by GitHub Actions from this repository. They come with SHA-256 checksums and a build
  provenance attestation (`gh attestation verify <file> --repo deymon28/CapsLangChange`).

## Uninstall

1. Tray > **Start with Windows** > **Off** (removes the registry value or scheduled task).
2. Tray > **Exit**, then delete the executable.
3. Optionally delete `%APPDATA%\CapsLangChange` (configuration), `%LOCALAPPDATA%\CapsLangChange` (logs) and the
   registry key `HKEY_CURRENT_USER\Software\CapsLangChange`.

## Building from source

Requirements: Rust (the version is pinned in `rust-toolchain.toml`) and Visual Studio Build Tools with the
"Desktop development with C++" workload (for the linker and the Windows SDK resource compiler).

```text
cargo test
cargo build --release
```

The executable is `target\release\CapsLangChange.exe`. `python tools/make_icons.py` regenerates the icons.
Design notes are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [docs/SPEC.md](docs/SPEC.md).

## License

[MIT](LICENSE)
