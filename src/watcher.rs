//! Watches the configuration directory and notifies the UI window when the file may have changed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::log;
use crate::util::wide;

/// Posted to the UI window after a change in the configuration directory (debounced).
pub const MSG_CONFIG_CHANGED: u32 = WM_APP + 41;

pub fn start(dir: &Path, ui: HWND) {
    let dir: PathBuf = dir.to_path_buf();
    let ui_addr = ui as usize;
    let spawned = std::thread::Builder::new().name("config-watcher".into()).spawn(move || watch(&dir, ui_addr as HWND));
    if let Err(e) = spawned {
        log::warning!("watcher", "cannot start the config watcher, use 'Reload config': {e}");
    }
}

fn watch(dir: &Path, ui: HWND) {
    // SAFETY: the directory handle stays open for the life of this thread (until process exit);
    // the buffer is DWORD-aligned and its size is passed.
    unsafe {
        let handle = CreateFileW(
            wide(dir).as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            log::warning!("watcher", "cannot watch the config folder: {}", std::io::Error::last_os_error());
            return;
        }
        let mut buf = vec![0u32; 1024];
        loop {
            let mut returned = 0u32;
            let ok = ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr().cast(),
                (buf.len() * 4) as u32,
                0,
                FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_SIZE,
                &mut returned,
                std::ptr::null_mut(),
                None,
            );
            if ok == 0 {
                log::warning!("watcher", "config watching stopped: {}", std::io::Error::last_os_error());
                return;
            }
            // Editors save in several steps; wait for them to finish. The UI compares the text,
            // so extra notifications are cheap.
            std::thread::sleep(Duration::from_millis(250));
            PostMessageW(ui, MSG_CONFIG_CHANGED, 0, 0);
        }
    }
}
