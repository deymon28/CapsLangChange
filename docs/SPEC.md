# CapsLangChange: technical specification

Status: implemented in 0.1.0; modifier taps added in 0.2.0; language indicator added after 0.2.0. Section 14 lists where the
implementation refined this specification; `docs/ARCHITECTURE.md` describes the code as built.

## 1. Purpose

A small background tray utility for Windows 10/11 that lets the user bind **any keyboard shortcut to an action**,
with **Caps Lock as a layout switch** as the headline use case (what PowerToys Keyboard Manager does with a global
`CapsLock -> Win+Space` remap), and one key difference: **every binding can be limited per program**, so games and
other programs that need a genuine Caps Lock receive it untouched.

Why not PowerToys Keyboard Manager itself: a single-key remap there is global; "Target App" only applies to
shortcuts. This tool is a standalone companion. It never reads, patches or depends on PowerToys files, so it is
unaffected by PowerToys updates. It is not a PowerToys plugin (Keyboard Manager has no extension API).

### Goals

- Fully user-controlled bindings: trigger shortcut -> action, each with optional per-program scope.
- Caps Lock tap switches the input layout by default (`Win+Space` method, switchable); reliable, no stuck keys.
- Works in elevated (administrator) windows too, like current PowerToys (which runs elevated), via an optional
  elevated autostart.
- Runs permanently from logon with near-zero CPU and a few MB of RAM.
- Manageable from the tray; configuration is a plain commented text file with validation.
- Portable for other users: one exe, no runtime, no admin rights needed for normal (non-elevated) use.

### Non-goals (v1)

- Launching programs or running commands from a binding (see 9.1: it would turn the config file into a privilege
  escalation path for the elevated mode).
- Mouse buttons as triggers, text macros, per-keyboard-device filtering.
- A full settings window (tray + config + recorder helper in v1; native dialog possible later).
- Touching or modifying PowerToys in any way.

## 2. Technology choice

| Option | Idle RAM | Distribution for other users | Stability / notes | Verdict |
| --- | --- | --- | --- | --- |
| **Rust + `windows-sys`** | ~2-4 MB | One static `.exe` (~0.4-0.8 MB), no runtime | Memory-safe, no GC pauses, precise control of the hook thread | **Chosen** |
| C# .NET 8 (WinForms tray) | 25-40 MB | Needs .NET runtime or a 10+ MB self-contained exe | GC and JIT; fine but heavier | Rejected |
| C# NativeAOT | ~8-15 MB | Single exe, ~3-5 MB | Good, but AOT/interop friction | Fallback |
| C / C++ | ~1-2 MB | Single exe | Smallest, but memory-unsafe code in a system-wide hook | Rejected |
| AutoHotkey v2 | ~6-10 MB | Needs AHK or a bundled interpreter exe, frequently flagged by antivirus and some anticheats | Easiest to write | Rejected |
| Python / Node | 20-60 MB | Interpreter or large bundle | Slow cold start, not appropriate for an always-on hook | Rejected |

(RAM figures are estimates; they are verified on the real build, see section 8.)

Crates (minimal; each reviewed for maintenance and `cargo audit` status):

- `windows-sys`: raw Win32 bindings (hooks, tray, registry, process query, input injection).
- `serde` + `toml_edit`: config parsing; `toml_edit` keeps the user's comments and layout when the tray rewrites
  the file.
- `embed-resource`: icons, version info and the application manifest.
- No async runtime, no GUI framework, no logging framework (a small rotating file logger).

Build profile: `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`, static CRT
(`+crt-static`) so no VC++ redistributable is needed.

Targets: `x86_64-pc-windows-msvc` (primary), `aarch64-pc-windows-msvc` (built in CI). Minimum OS: Windows 10 1809.
Build requirements: the Rust toolchain pinned in `rust-toolchain.toml` and Visual Studio Build Tools (C++ workload).

## 3. Architecture

```
 main thread (UI)                      hook thread (input)                         watcher thread
 ----------------                      -------------------                         --------------
 message-only window                   WH_KEYBOARD_LL + GetMessage loop            ReadDirectoryChangesW on the
 Shell_NotifyIcon tray                 own modifier table, key-bitmap filter       config directory, debounce,
 popup menu (modal loop)               engine decision (pure code)                 parse + validate, atomic swap
 SetWinEventHook(FOREGROUND)           swallow / pass; enqueue action
 config writes (toml_edit)             never allocates on the hot path
```

- The hook lives on its own thread because `TrackPopupMenu` runs a modal loop; on a shared thread the keyboard would
  stall while the tray menu is open.
- Shared state: one `Arc<Snapshot>` (compiled bindings, exclusions, options) swapped atomically on reload, plus a few
  atomics (enabled flag, foreground-excluded cache). No mutex inside the hook callback.
- `engine.rs` is pure Rust with no Win32 types: it receives `(vk, scan, down, injected_by_us, modifier_state,
  foreground_exe)` and returns `Pass | Swallow(action)`. Everything important is unit-tested there.
- Actions that inject input run synchronously from the hook thread right after the decision, in order, so injected
  events cannot interleave with later physical events (see 4.4).

Hook robustness:

- The callback cannot panic outward: the hot path is allocation-free, indexing-free branch logic covered by tests;
  `panic = "abort"` is a last resort rather than a plan.
- Non-trigger keys cost one bitmap lookup (a bitmap of all virtual-key codes that appear as a trigger or modifier).
- Re-install the hook after resume from sleep, session unlock, and when a self-test event fails to arrive
  (Windows silently removes a hook that exceeds its time limit).

## 4. Behaviour specification

### 4.1 Bindings

A binding is `trigger -> action` plus optional scope. Evaluated on key **down**; the matching key-up is swallowed
too (decision remembered per key, so a key-up never arrives without its key-down and the decision cannot flip if
the foreground window changes between down and up). Auto-repeat of a swallowed key is swallowed and ignored.

Trigger syntax (`keys`): modifiers and one main key joined by `+`, case-insensitive.

- Modifiers: `Ctrl`, `Alt`, `Shift`, `Win` (either side); `LCtrl`, `RCtrl`, `LAlt`, `RAlt`, `LShift`, `RShift`,
  `LWin`, `RWin` (specific side).
- Main key: letters, digits, `F1`-`F24`, `Space`, `Tab`, `Enter`, `Esc`, `Backspace`, `CapsLock`, `Insert`, `Delete`,
  `Home`, `End`, `PageUp`, `PageDown`, arrows, numpad keys, punctuation names (`Grave`, `Minus`, ...), or
  `vk:0xNN` for anything else.
- Matching is **exact on modifiers**: binding `CapsLock` fires only when no modifier is held, so `Shift+CapsLock`
  reaches the system as a real Caps Lock unless it is bound. If several bindings match, the one with the most
  modifiers wins; conflicting duplicates are reported by validation.

Actions:

| `action` | Parameters | Effect |
| --- | --- | --- |
| `switch_layout` | `mode = "next" \| "previous" \| "set"`; `layout` (for `set`: KLID like `"00000419"`); `cycle = [...]` optional list restricting the cycle; `method = "win_space" \| "message"` (default from global option) | Change input layout |
| `capslock` | `state = "toggle" \| "on" \| "off"` | Change the real Caps Lock toggle state |
| `send_keys` | `send = "Ctrl+C"` | Replace the trigger with another shortcut (KBM-style remap) |
| `block` | none | Swallow the shortcut, do nothing |
| `passthrough` | none | Explicitly let the shortcut through (useful to carve exceptions with scope) |

Scope per binding: `apps_only = ["exe", ...]` (fire only there), `apps_except = ["exe", ...]` (never fire there),
`enabled = true|false`. A global exclusion list (`[[exclude]]`, edited from the tray) suspends **all** bindings in
those programs, so a game needs one entry, not one per binding.

### 4.2 Layout switching methods

1. `win_space` (default): inject `Win down, Space down, Space up, Win up`, tagged with our marker. Identical to what
   Keyboard Manager does today, works in every window, shows the standard Windows language popup, cycles all
   installed layouts (for example EN `00000409`, RU `00000419`, UK `00000422`).
2. `message`: pick the next/previous/specific layout from `GetKeyboardLayoutList` and post
   `WM_INPUTLANGCHANGEREQUEST` to the foreground window. No synthetic Win key, no modifier juggling. Supports `cycle`
   to toggle only between chosen layouts. Known weaker spots: some console/UWP windows (documented).

Both methods are global defaults overridable per binding, so one shortcut can cycle and another can jump straight to
a specific language.

### 4.3 Modifier masking

When a combination is swallowed, the system has seen modifier presses without any key, which has side effects: a lone
`Win` release opens the Start menu, a lone `Alt` release activates the menu bar. Before a swallowed chord's modifier
is released, the tool injects a harmless unassigned key press (`VK 0xE8`, tagged) so the system treats the sequence
as used. Required for chords like `Win+Ctrl+Shift+Space -> capslock`.

### 4.4 Injection and held modifiers

An injected action must not combine with modifiers the user is physically holding (for example `Win+Space` while
`Ctrl+Alt+Shift` are held would be a different shortcut). The injector therefore: (1) injects key-ups for held
modifiers, (2) injects the action, (3) re-presses the modifiers that are still physically held, (4) after about
30 ms reconciles against the physical modifier table and releases any modifier that was re-pressed after the user had
already let go (prevents stuck modifiers). Physical state comes from the tool's own table built from the hook event
stream (our tagged events excluded), resynchronised with `GetAsyncKeyState` on start, resume, unlock and foreground
change; events injected by other software are treated as normal input.

### 4.5 Caps Lock toggle-state restore

A genuine Caps Lock press in an excluded program flips the global toggle state; without correction the user would
return to the desktop typing in capitals. When the foreground changes from an excluded program to a non-excluded one
and the Caps Lock toggle is on, the tool injects one tagged Caps Lock press to switch it off
(`restore_capslock_on_leave = true`). Detection: `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)`.

### 4.6 Program matching

Entries are matched against the foreground window's process:

- `exe` (file name, case-insensitive): the common case;
- optional `path` prefix and `class` (window class) for disambiguation.

Lookup happens only when a binding trigger matches: `GetForegroundWindow` -> `GetWindowThreadProcessId` ->
`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` -> `QueryFullProcessImageNameW`. If the process cannot be opened the
result is "unknown", treated as not excluded (`on_lookup_failure = "act" | "pass"` to change it).

## 5. Tray UI

Icon states: active, paused, "idle in excluded program". Tooltip e.g. `CapsLangChange: active`,
`paused`, `inactive in game.exe`. Menu:

```
 [x] Enabled (pause = everything passes through)
 Bindings            >  [x] CapsLock -> switch layout        (check/uncheck to enable)
                        [x] Ctrl+Alt+Shift+Space -> capslock
                        ...
 Layout method       >  Win+Space (default) / Message
 Excluded programs   >  game.exe  (click to remove)
                        ----
                        Add running program...      (visible-window processes)
                        Add next active program     (5 s capture: Alt+Tab into the game and wait)
 Record a shortcut...   (press a combination; its config text, e.g. Win+Ctrl+Shift+Space, goes to the clipboard)
 [x] Restore Caps Lock state when leaving excluded program
 Start with Windows  >  Off / Normal / Elevated (asks for administrator rights once)
 Open config file / Reload config / Check config / Open logs folder
 About / version
 Exit
```

"Add next active program" and "Record a shortcut" exist because a fullscreen game hides the tray and because key names
are easier copied than typed. Balloon notifications confirm what was captured. First run: a balloon "Running in the
tray" and a commented default config.

## 6. Configuration

Location: `%APPDATA%\CapsLangChange\config.toml`. Hot-reloaded; an invalid file keeps the last good config and shows
the error in a balloon and the log. `capslangchange --check-config` validates without running.

Shipped default (generic, no personal data): Caps Lock switches the layout; `Shift+CapsLock` needs no binding.

```toml
# CapsLangChange configuration
enabled = true
method = "win_space"               # "win_space" | "message"
restore_capslock_on_leave = true
on_lookup_failure = "act"          # "act" | "pass"

[[binding]]
name = "Switch layout"
keys = "CapsLock"
action = "switch_layout"           # mode defaults to "next"

# Examples (uncomment / adapt):
# [[binding]]
# name = "Real Caps Lock"
# keys = "Ctrl+Alt+Shift+Space"
# action = "capslock"
#
# [[binding]]
# keys = "Win+Ctrl+Shift+1"
# action = "switch_layout"
# mode = "set"
# layout = "00000409"
# method = "message"

# [[exclude]]
# exe = "SomeGame.exe"
```

Personal configurations live only in `%APPDATA%` and are never part of the repository. The tray writes through `toml_edit`; only program names and shortcut text are stored.

## 7. Startup, lifecycle, logging

- Autostart modes (tray toggle, only one active at a time):
  - **Normal**: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, quoted exe path plus `--autostart`. No admin.
  - **Elevated**: Task Scheduler task "CapsLangChange" for the current user, trigger "at logon", run level highest,
    no execution time limit, battery-safe. Created once via an elevated `schtasks /Create /XML` (one UAC prompt).
    Needed to work in administrator windows, because a non-elevated process does not receive keyboard events while an
    elevated window has focus (UIPI). PowerToys offers "run as administrator" for the same reason.
  - On every launch the stored path is refreshed if the exe moved.
- Single instance via a named mutex; a second launch asks the first to show a balloon (cross-integrity message allowed
  with `ChangeWindowMessageFilterEx`) and exits.
- Clean exit: unhook, remove the tray icon, flush the log.
- Conflict check: if `PowerToys.KeyboardManagerEngine.exe` is running, show a one-time balloon recommending to remove
  its Caps Lock remaps. Never changes PowerToys.
- Logging: `%LOCALAPPDATA%\CapsLangChange\logs\capslangchange.log`, size-capped rotation (about 512 KB x 2), levels
  `error`/`info`/`debug` (`--debug`). Logs lifecycle events, reloads and which binding fired; **never key codes of
  non-matching keys or anything typed**.
- CLI: `--debug`, `--config <path>`, `--check-config`, `--autostart`, `--version`, `--help`.

## 8. Resource budget and verification targets

To be measured on the real build, not assumed: idle CPU about 0%; private working set under about 5 MB; exe under
1 MB; hook callback for a non-trigger key in single-digit microseconds; trigger path under about 1 ms including the
process lookup; no handle/memory growth over 24 hours.

## 9. Security, privacy and compatibility

### 9.1 Privacy and security

- The hook sees every virtual-key code (needed to track modifiers) but stores, logs and transmits none of them.
  Nothing leaves the machine: no network access, no telemetry, no auto-update in v1.
- No injection into other processes, no reading of other processes' memory. Only the foreground process image path is
  queried, with the least access right.
- **Elevated mode raises the stakes**: a process with administrator rights that reads a user-writable config could be
  steered by malware that can write that config, and a user-writable exe could be replaced. Therefore: no
  program-launching action exists in v1; `send_keys` and `capslock` only synthesise keys; the tray warns when the
  exe's directory is writable by a normal user while elevated autostart is enabled and recommends a protected
  location such as `C:\Program Files\CapsLangChange`.
- Antivirus note: any low-level keyboard hook can resemble a keylogger to heuristics. Mitigations: open source,
  CI-built releases with SHA-256 checksums and provenance attestation, README section. The binary is **unsigned**
  unless the author obtains a certificate; SmartScreen may warn.

### 9.2 Games and anticheat

The tool never touches the game process. In an excluded program it injects nothing and the key passes through
unchanged. Its hook is the same class as the Keyboard Manager hook already installed. Some anticheat products dislike
*any* third-party hook; this can only be verified in the actual game (Arena Breakout Infinite), so it is a listed
risk, with a possible "strict mode" (remove the hook entirely while an excluded program is in the foreground) as the
fallback.

## 10. Repository and delivery

```
CapsLangChange/
  Cargo.toml  Cargo.lock  rust-toolchain.toml  .cargo/config.toml  build.rs
  assets/            icons (active / paused / excluded), manifest, rc file
  src/
    main.rs          startup, single instance, thread wiring
    engine.rs        pure binding engine + modifier table + matching (no Win32; unit-tested)
    keys.rs          key / shortcut text parsing and formatting
    hook.rs          WH_KEYBOARD_LL thread, tag marker, key bitmap
    inject.rs        SendInput sequences, masking, held-modifier handling, reconciliation
    layout.rs        win_space / message strategies
    process.rs       foreground process lookup, running-program enumeration
    config.rs        schema, defaults, validation, toml_edit writes, file watcher
    tray.rs          window, icon, menu, balloons, capture and recorder helpers
    autostart.rs     Run key and scheduled task management
    log.rs           small rotating logger
  docs/SPEC.md  docs/ARCHITECTURE.md (after implementation)
  README.md  LICENSE (MIT)  CHANGELOG.md
  .github/workflows/ci.yml  release.yml
```

- English only for code, comments, docs and UI text.
- `AGENTS.md`, `CLAUDE.md`, `.claude/` etc. are local-only via `.git/info/exclude`.
- CI (`windows-latest`): `cargo fmt --check`, `clippy -D warnings`, `cargo test`, `cargo audit`, release build;
  default `contents: read`, actions pinned to commit SHAs. Release on a version tag: x64 and arm64 builds with
  `--remap-path-prefix`, exe + zip + `SHA256SUMS`, provenance attestation. Published binaries come from CI only.
- Build output must not leak machine data: `strip = true`, path remapping, and a post-build scan of the exe for the
  local username and drive paths.
- Commits carry no personal e-mail: repository-local `user.name`/`user.email` set to a non-identifying value agreed
  with the author before the first commit; no AI attribution trailers.
- README: what it does, install, first run, binding and exclusion examples, key-name reference, elevated mode,
  removing the PowerToys Caps Lock remap, troubleshooting, limitations, uninstall.

## 11. Testing

- Unit tests (`engine.rs`, `keys.rs`, `config.rs`): down/up pairing, auto-repeat, foreground change between down and
  up, exact-modifier matching, most-specific-wins, scope (`apps_only`/`apps_except`/global exclude), disabled state,
  own-injection ignore, masking decisions, held-modifier injection plan, restore-state decision table, key-name
  parsing round-trips, invalid and conflicting config.
- Manual smoke matrix: Notepad, browser, Windows Terminal, an elevated window (with elevated autostart), UWP app,
  remote desktop, fullscreen game; sleep/resume; lock/unlock; fast user switching; multi-monitor; two and three
  layouts; reboot autostart in both modes; PowerToys conflict warning; stuck-modifier stress (rapid chords).
- Measurement: working set, CPU and handle counts over time.
- Not verifiable by the implementer: behaviour inside Arena Breakout Infinite with its anticheat; the user performs
  the final check and the implementation reports this limitation.

## 12. Milestones

0. Local git, local-only exclusion of agent files, project skeleton, toolchain check (Rust 1.99 MSVC present).
1. `keys.rs` and `engine.rs` with tests (no Win32).
2. Hook thread, injection, `switch_layout` (both methods) and `capslock`; first real Caps Lock test.
3. Config, bindings, scope, exclusions, process lookup, restore-state, foreground events.
4. Tray, menu, capture and recorder helpers, autostart (normal and elevated), single instance, logging.
5. Measurement, hardening, README and docs, CI and release workflows.
6. Pre-publish audit (tracked files, history, personal data, non-English text, commit identity, binary strings),
   then commit and push to the GitHub repository.

## 13. Decisions recorded from review

- Layout switching: `Win+Space` by default, switchable globally and per binding; cycle-all by default, restricted
  cycles configurable.
- Real Caps Lock: not hard-coded; any shortcut can be bound to `capslock` (for example the
  `Ctrl+Alt+Shift+Space` chord often used with Keyboard Manager).
- Any shortcut -> action bindings, per-program scope, tray management.
- Elevated windows must work (as with PowerToys running as administrator).
- License MIT; no personal e-mail in commits.

## 14. Refinements made during implementation

- Matching uses the logical modifier state (`GetAsyncKeyState`, evaluated only for candidate keys). The engine's own
  physical table (built from external events) is used for restore decisions and AltGr handling.
- Stuck-modifier prevention: instead of a timed reconciliation pass, re-pressed modifiers carry a separate tag and the
  engine drops them if the physical key is already up (see ARCHITECTURE.md). Verified with injected held chords.
- `capslock` actions do not lift held modifiers; `message` switching injects nothing.
- `mode = "set"` and `cycle` always use the `message` method; combining them with an explicit `win_space` is a
  configuration error.
- Caps Lock restore returns to the state remembered when the excluded program became active (not simply "off").
- Program lookup falls back to the Toolhelp process list when a process refuses limited query access.
- Hook watchdog: re-install after 10 minutes without key events while `GetLastInputInfo` shows user activity, plus on
  unlock and resume; no periodic self-test injection (it would send input into excluded programs).
- The elevated-autostart warning checks whether the executable is inside the user profile (no ACL evaluation).
- Autostart mode and one-time notices are remembered under `HKCU\Software\CapsLangChange`.
- An autostart instance waits up to 15 seconds for the single-instance mutex (normal-to-elevated hand-over).
- Modifier taps (from section 15): a single modifier as `keys` fires on release if it was pressed alone, released
  within 500 ms, and no other key, mouse button or wheel was used meanwhile. The modifier is never swallowed on
  key-down; for Win and Alt the key-up is replaced by the mask key plus an injected key-up. A `WH_MOUSE_LL` hook exists
  only while such a binding is configured. The recorder also records taps and masks recorded Win/Alt chords.
- `layout` without `mode` implies `mode = "set"`. Settings without effect for the action, and layout ids that are
  not installed, produce warnings.
- Language indicator (added later): after a `switch_layout` binding, a click-through capsule next to the text caret
  (or the mouse pointer) shows the two-letter language code of the new layout, or an own label per layout id. It
  stays while the trigger key is held and for `indicator_duration_ms` afterwards. Settings: `indicator`,
  `indicator_position`, `indicator_duration_ms` (500), `indicator_height` (15 px at 96 DPI), `indicator_labels`;
  tray toggle. It runs on its own thread; the hook only posts messages to it. Layout changes made outside CapsLangChange are not shown.
- Settings introduced after the first release (`config::ADDED_SETTINGS`) are added to an existing file on start,
  with the comments of the default configuration (`toml_edit`, the rest of the file unchanged).

## 15. Possible later additions

Modifier chords without a main key (for example `LShift+RShift`); a native settings dialog; "strict mode"; automatic
fullscreen exclusion; mouse-button triggers; winget package; `run_program` action (only with a safer elevated design).
