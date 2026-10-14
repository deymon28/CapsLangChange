# Architecture

CapsLangChange is a single Rust executable (`windows` subsystem, no console) built on raw Win32 calls
(`windows-sys`). The design goals are a hook that never delays the keyboard, no stuck keys, and a small,
testable core.

## Threads

| Thread | Owns | Talks to others by |
| --- | --- | --- |
| UI (main) | hidden top-level window, tray icon and menu, foreground `WinEvent` hook, session/power notifications, config edits | posts thread messages to the hook thread |
| keyboard-hook | `WH_KEYBOARD_LL` hook (plus `WH_MOUSE_LL` while a modifier-tap binding exists) and its message loop, the `Engine`, input injection | posts window messages to the UI (recorded shortcut) |
| config-watcher | `ReadDirectoryChangesW` on the config folder | posts `MSG_CONFIG_CHANGED` to the UI |
| short-lived helper threads | waiting for the elevated helper process (UAC) | post `MSG_HELPER_DONE` to the UI |

The hook has its own thread because the tray menu (`TrackPopupMenu`) and message boxes run modal loops. A hook on the
UI thread would stall the whole keyboard while a menu is open.

New configurations reach the hook thread as a boxed `Arc<Config>` in a thread message. The hook callback never takes
a lock.

## Modules

| Module | Role | Win32 |
| --- | --- | --- |
| `keys` | key names, modifier patterns, shortcut parsing and formatting | no |
| `config` | TOML schema, validation with readable errors, priority compilation, comment-preserving edits (`toml_edit`) | no (file IO only) |
| `engine` | per-event decision: pass, swallow, which binding runs; modifier taps; shortcut recorder | no |
| `hook` | hook thread, origin tagging, watchdog, action execution | yes |
| `inject` | `SendInput` sequences, mask key, held-modifier lift and restore | yes |
| `layout` | `WM_INPUTLANGCHANGEREQUEST` switching, HKL to KLID mapping, layout listing | yes |
| `process` | foreground program identification, process enumeration | yes |
| `tray` | UI thread: tray, menu, notifications, reloads, exclusions, Caps Lock restore, autostart UI | yes |
| `autostart` | Run registry value, scheduled task XML and `schtasks`, token queries | yes |
| `watcher`, `log`, `util` | config watching, rotating log file, string helpers | yes |

## Event flow

1. The hook receives a `KBDLLHOOKSTRUCT`. Events injected by CapsLangChange carry `dwExtraInfo = TAG_NORMAL` or
   `TAG_RESTORE` and are classified as `Origin::Ours` or `Origin::OursRestore`.
2. `Engine::on_key` updates its physical modifier table for external modifier events, then decides:
   - Modifiers are never swallowed.
   - A key-up is swallowed exactly when its key-down was. Auto-repeat of a swallowed key is swallowed and only re-runs
     a binding with `repeat = true`.
   - For a key-down, only bindings for that virtual-key code are considered (`Config::candidates`, pre-sorted by
     priority). The logical modifier state (`GetAsyncKeyState`) and the foreground program are evaluated lazily, only
     if a candidate exists, so ordinary typing costs one table lookup.
   - Global exclusions and the per-binding scope decide which binding, if any, applies.
3. Modifier taps: a modifier pressed while no other modifier is physically held becomes a tap candidate. Any
   other key event, another modifier, a mouse click or wheel (`WH_MOUSE_LL`, installed only while a tap binding exists)
   or a press longer than `TAP_MAX_MS` (500 ms) cancels it. On its key-up the engine looks up the bindings stored under
   `keys::TAP` (virtual-key code 0) and returns `Tap`. The modifier itself is never swallowed on key-down. For Win and
   Alt the hook swallows the physical key-up and injects the mask key followed by the key-up instead, because a mask
   injected while the hook processes the key-up would reach the system only after it (Start menu, menu bar). The key-up
   is therefore always delivered.
4. The shortcut recorder lives in the engine too: while recording, the next non-modifier key-down (swallowed together
   with its key-up) or modifier tap is returned as `Recorded`, with the same masking.
5. On `Swallow { mask, run, held }` the hook first injects the mask key (`VK 0xE8`) when modifiers are held, so the
   swallowed chord's Win/Alt release does not open the Start menu or menu bar or trigger the Windows language hotkey.
   It then executes the action synchronously, so the injected events stay in order relative to later physical input.

## Injection and stuck-key prevention

`inject::shortcut` releases the modifiers the user holds, presses the target shortcut, and re-presses the held
modifiers with `TAG_RESTORE`. If the user released a modifier before the re-press is processed, the engine sees
`OursRestore` for a modifier its physical table marks as up and drops it. This is the only way an injected modifier
press could outlive the physical key, so modifiers cannot get stuck.

`capslock` actions tap `VK_CAPITAL` without lifting modifiers (Caps Lock toggles regardless of them), and `message`
layout switching injects nothing at all.

AltGr produces a synthetic Left Ctrl (scan code `0x21D`). The engine ignores it for the physical table and removes it
from the held state while Right Alt is down, so `RAlt+...` bindings work on layouts with AltGr.

## Foreground tracking and Caps Lock restore

The UI thread listens to `EVENT_SYSTEM_FOREGROUND` (events for its own windows are skipped, and while its own window
is in front, for example the tray menu, the last other program still counts). On entering a globally excluded program it stores the Caps Lock
toggle state. On leaving, it taps Caps Lock if the state differs and `restore_capslock_on_leave` is set. The tray icon
and tooltip reflect the state.

Program identification uses `QueryFullProcessImageNameW` with `PROCESS_QUERY_LIMITED_INFORMATION`, with a fallback
to the Toolhelp process list for protected processes.

## Robustness

- Windows silently removes a low-level hook that once exceeds its time-out. A 60-second watchdog re-installs the hook
  when the user was active (`GetLastInputInfo`) but no key event arrived for 10 minutes. It is also re-installed on
  session unlock, on returning to the session (fast user switching, Remote Desktop) and on resume from sleep, which also resynchronises the modifier table.
- The hook thread runs at `THREAD_PRIORITY_HIGHEST` so a busy CPU (games) does not delay typing.
- Single instance: a named mutex in the session namespace. An instance started by autostart waits up to 15 seconds,
  which allows the hand-over from a normal to an elevated instance.
- Explorer restarts are handled through the `TaskbarCreated` message.
- `panic = "abort"`: if anything ever panics, the process ends, Windows removes the hook, and the keyboard keeps working
  normally. The panic message is logged first.

## Elevated autostart

The tray starts the executable again with the `runas` verb and `--register-task <SID>` (one UAC prompt). That elevated
helper writes a task XML (logon trigger for the SID, `HighestAvailable`, no time limit, normal priority), creates it
with `schtasks`, and starts it. The normal instance then exits, and the elevated instance takes over the single-instance
mutex. Removing the task works the same way with `--unregister-task`. The chosen mode is remembered in
`HKCU\Software\CapsLangChange\Autostart`.

There is deliberately no action that launches programs: an elevated process reading a user-writable configuration
must not become a way to run code with administrator rights.

## Testing

`cargo test` covers the pure modules: shortcut parsing and formatting round-trips, exact modifier matching,
configuration parsing and error messages, comment-preserving edits, binding priority, the engine's pairing,
auto-repeat, scope, exclusion, restore-drop and AltGr rules, modifier taps and the recorder, warnings for settings
without effect and for layouts that are not installed, autostart XML escaping, and process lookup.

Win32 behaviour was verified manually on Windows 11 with injected input: layout switching with both methods, Caps Lock
actions, modifier restore during held chords, exclusions, hot reload, single instance, and clean exit. Elevated
autostart, the Caps Lock restore on foreground change, and behaviour inside games need checks on a real desktop session.
