//! Small size-capped rotating file logger.
//!
//! Never log key codes of ordinary typing or any typed text.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};

const MAX_BYTES: u64 = 512 * 1024;

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static DEBUG: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
}

/// Opens (or creates) the log file. Logging silently does nothing if this fails.
pub fn init(path: PathBuf, debug: bool) {
    DEBUG.store(debug, Ordering::Relaxed);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        if let Ok(mut sink) = SINK.lock() {
            *sink = Some(Sink { path, file, written });
        }
    }
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

pub fn write(level: Level, component: &str, message: &str) {
    if matches!(level, Level::Debug) && !debug_enabled() {
        return;
    }
    let line = format!("{} {} {component}: {message}\r\n", timestamp(), level.as_str());
    let Ok(mut guard) = SINK.lock() else { return };
    let Some(sink) = guard.as_mut() else { return };
    if sink.written + line.len() as u64 > MAX_BYTES {
        rotate(sink);
    }
    if sink.file.write_all(line.as_bytes()).is_ok() {
        sink.written += line.len() as u64;
    }
}

fn rotate(sink: &mut Sink) {
    let old = sink.path.with_extension("log.1");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(&sink.path, &old);
    if let Ok(file) = OpenOptions::new().create(true).write(true).truncate(true).open(&sink.path) {
        sink.file = file;
        sink.written = 0;
    }
}

fn timestamp() -> String {
    // SAFETY: plain out-parameter calls.
    let (t, bias) = unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let bias = match GetTimeZoneInformation(&mut tz) {
            2 => tz.Bias + tz.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            0xFFFF_FFFF => 0,
            _ => tz.Bias + tz.StandardBias,
        };
        (t, bias)
    };
    let offset = -bias;
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.abs();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {sign}{:02}:{:02}",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond,
        t.wMilliseconds,
        offset / 60,
        offset % 60
    )
}

macro_rules! info {
    ($component:expr, $($arg:tt)*) => { $crate::log::write($crate::log::Level::Info, $component, &format!($($arg)*)) };
}
macro_rules! warning {
    ($component:expr, $($arg:tt)*) => { $crate::log::write($crate::log::Level::Warn, $component, &format!($($arg)*)) };
}
macro_rules! error {
    ($component:expr, $($arg:tt)*) => { $crate::log::write($crate::log::Level::Error, $component, &format!($($arg)*)) };
}
macro_rules! debug {
    ($component:expr, $($arg:tt)*) => {
        if $crate::log::debug_enabled() {
            $crate::log::write($crate::log::Level::Debug, $component, &format!($($arg)*))
        }
    };
}
pub(crate) use {debug, error, info, warning};
