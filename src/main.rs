//! CapsLangChange: Caps Lock layout switching and per-program keyboard shortcut bindings for Windows.

#![windows_subsystem = "windows"]

mod autostart;
mod config;
mod engine;
mod hook;
mod inject;
mod keys;
mod layout;
mod log;
mod process;
mod tray;
mod util;
mod watcher;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING, WriteFile};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, WriteConsoleW,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, PostMessageW, RegisterWindowMessageW,
};

use crate::util::wide;

const USAGE: &str = "CapsLangChange - Caps Lock layout switching and per-program keyboard shortcuts.

Usage: CapsLangChange.exe [options]

  --config <path>   use this configuration file instead of %APPDATA%\\CapsLangChange\\config.toml
  --check-config    validate the configuration file and exit (exit code 0 = valid)
  --debug           write detailed log messages
  --version         print the version and exit
  --help            print this help and exit

Without options the program starts in the tray. Settings: right-click the tray icon.";

#[derive(Default)]
struct Args {
    config: Option<PathBuf>,
    check_config: bool,
    debug: bool,
    autostart: bool,
    version: bool,
    help: bool,
    register_task: Option<String>,
    unregister_task: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_string_lossy().as_ref() {
            "--config" => args.config = Some(it.next().ok_or("--config needs a file path")?.into()),
            "--check-config" => args.check_config = true,
            "--debug" => args.debug = true,
            "--autostart" => args.autostart = true,
            "--version" | "-V" => args.version = true,
            "--help" | "-h" | "/?" => args.help = true,
            // Internal: used by the elevated helper process started from the tray menu.
            "--register-task" => {
                args.register_task = Some(it.next().ok_or("--register-task needs a SID")?.to_string_lossy().into())
            }
            "--unregister-task" => args.unregister_task = true,
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    Ok(args)
}

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            output(&format!("{e}\n\n{USAGE}"), true);
            return 2;
        }
    };
    if args.help {
        output(USAGE, false);
        return 0;
    }
    if args.version {
        output(&format!("CapsLangChange {}", env!("CARGO_PKG_VERSION")), false);
        return 0;
    }

    let config_path = args.config.clone().unwrap_or_else(default_config_path);
    if args.check_config {
        return check_config(&config_path);
    }

    let logs = local_data_dir().join("logs");
    log::init(logs.join("capslangchange.log"), args.debug);
    std::panic::set_hook(Box::new(|info| log::error!("panic", "{info}")));

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            output(&format!("Cannot determine the program path: {e}"), true);
            return 1;
        }
    };

    if let Some(sid) = args.register_task {
        return match autostart::register_task(&exe, &sid, true) {
            Ok(()) => {
                log::info!("autostart", "administrator logon task created and started");
                0
            }
            Err(e) => {
                log::error!("autostart", "creating the administrator logon task failed: {e}");
                1
            }
        };
    }
    if args.unregister_task {
        return match autostart::unregister_task() {
            Ok(()) => {
                log::info!("autostart", "administrator logon task removed");
                0
            }
            Err(e) => {
                log::error!("autostart", "removing the administrator logon task failed: {e}");
                1
            }
        };
    }

    // Autostart instances wait a little: an elevated instance may be taking over from a normal one.
    let Some(_instance) = acquire_instance(args.autostart) else {
        if !args.autostart {
            notify_running_instance();
        }
        return 0;
    };

    log::info!("main", "CapsLangChange {} starting", env!("CARGO_PKG_VERSION"));
    let created = !config_path.exists();
    let text = match config::load_or_create(&config_path) {
        Ok(t) => t,
        Err(e) => {
            log::error!("main", "cannot read or create {}: {e}", config_path.display());
            tray::fatal(&format!("Cannot read or create the configuration file:\n{}\n\n{e}", config_path.display()));
            return 1;
        }
    };
    if created {
        log::info!("main", "created the default configuration at {}", config_path.display());
    }
    let config = match layout::parse_config(&text) {
        Ok(c) => c,
        Err(errors) => {
            for e in &errors {
                log::error!("config", "{e}");
            }
            // Start without bindings (keys stay untouched); fixing the file applies it automatically.
            tray::fatal(&format!(
                "The configuration file has errors, so no keys are changed until it is fixed:\n{}\n\n{}",
                config_path.display(),
                errors.join("\n")
            ));
            config::parse("").expect("empty configuration is valid")
        }
    };

    let paths = tray::Paths { config: config_path, logs, exe };
    let startup = tray::Startup { config_created: created };
    tray::run(paths, text, Arc::new(config), startup)
}

fn default_config_path() -> PathBuf {
    roaming_data_dir().join("config.toml")
}

fn roaming_data_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("CapsLangChange")
}

fn local_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(roaming_data_dir_parent).join("CapsLangChange")
}

fn roaming_data_dir_parent() -> PathBuf {
    roaming_data_dir().parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn check_config(path: &std::path::Path) -> i32 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            output(&format!("Cannot read {}: {e}", path.display()), true);
            return 1;
        }
    };
    match layout::parse_config(&text) {
        Ok(c) => {
            let mut msg = format!(
                "{}: valid ({} bindings, {} excluded programs)",
                path.display(),
                c.bindings.len(),
                c.excludes.len()
            );
            for w in &c.warnings {
                msg.push_str(&format!("\nwarning: {w}"));
            }
            output(&msg, false);
            0
        }
        Err(errors) => {
            let mut msg = format!("{}: invalid", path.display());
            for e in &errors {
                msg.push_str(&format!("\nerror: {e}"));
            }
            output(&msg, true);
            1
        }
    }
}

/// Holds the single-instance mutex for the life of the process.
struct Instance(HANDLE);

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: handle owned by this guard.
        unsafe { CloseHandle(self.0) };
    }
}

fn acquire_instance(wait: bool) -> Option<Instance> {
    let name = wide("Local\\CapsLangChange.Instance");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // SAFETY: name outlives the call; the handle is owned by the returned guard or closed.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            let already = GetLastError() == ERROR_ALREADY_EXISTS;
            if !handle.is_null() && !already {
                return Some(Instance(handle));
            }
            // Either it exists, or an elevated instance owns it and access is denied.
            if !handle.is_null() {
                CloseHandle(handle);
            }
        }
        if !wait || Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn notify_running_instance() {
    // SAFETY: strings outlive the calls.
    unsafe {
        let hwnd = FindWindowW(wide(tray::WINDOW_CLASS).as_ptr(), wide(tray::WINDOW_TITLE).as_ptr());
        if !hwnd.is_null() {
            let msg = RegisterWindowMessageW(wide(tray::ALREADY_RUNNING_MESSAGE).as_ptr());
            PostMessageW(hwnd, msg, 0, 0);
        }
    }
}

/// Prints to the console the program was started from (or redirected output), else shows a message box.
fn output(text: &str, error: bool) {
    // SAFETY: handles are checked; buffers outlive the calls.
    unsafe {
        let mut handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut opened = false;
        if (handle.is_null() || handle == INVALID_HANDLE_VALUE) && AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            handle = CreateFileW(
                wide("CONOUT$").as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            opened = handle != INVALID_HANDLE_VALUE;
        }
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            let line = format!("{text}\r\n");
            let mut mode = 0;
            let mut written = 0u32;
            let ok = if GetConsoleMode(handle, &mut mode) != 0 {
                let w: Vec<u16> = line.encode_utf16().collect();
                WriteConsoleW(handle, w.as_ptr().cast(), w.len() as u32, &mut written, std::ptr::null()) != 0
            } else {
                WriteFile(handle, line.as_ptr(), line.len() as u32, &mut written, std::ptr::null_mut()) != 0
            };
            if opened {
                CloseHandle(handle);
            }
            if ok {
                return;
            }
        }
        let flags = if error { MB_OK | MB_ICONERROR } else { MB_OK | MB_ICONINFORMATION };
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("CapsLangChange").as_ptr(), flags);
    }
}
