//! Foreground program identification and process enumeration.

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible,
};
use windows_sys::core::BOOL;

use crate::config::AppInfo;
use crate::util::from_wide;

/// The program owning the foreground window, or `None` if it cannot be identified.
pub fn foreground_app() -> Option<AppInfo> {
    // SAFETY: plain query.
    let hwnd = unsafe { GetForegroundWindow() };
    app_for_window(hwnd)
}

thread_local! {
    /// Last foreground lookup: (window, process id, result).
    static FOREGROUND_CACHE: RefCell<Option<(usize, u32, Option<AppInfo>)>> = const { RefCell::new(None) };
}

/// Like `foreground_app`, but reuses the previous result while the same window of the same process
/// stays in the foreground (keeps the keyboard hook fast, also for protected processes).
pub fn foreground_app_cached() -> Option<AppInfo> {
    // SAFETY: plain queries.
    let (hwnd, pid) = unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (hwnd, pid)
    };
    if let Some(hit) = FOREGROUND_CACHE.with(|c| {
        c.borrow().as_ref().filter(|(h, p, _)| *h == hwnd as usize && *p == pid).map(|(_, _, app)| app.clone())
    }) {
        return hit;
    }
    let app = app_for_window(hwnd);
    FOREGROUND_CACHE.with(|c| *c.borrow_mut() = Some((hwnd as usize, pid, app.clone())));
    app
}

pub fn app_for_window(hwnd: HWND) -> Option<AppInfo> {
    if hwnd.is_null() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return None;
    }
    // Some protected processes (for example games with an anticheat) refuse even limited access;
    // the process list still reveals the executable name.
    let path = image_path(pid).or_else(|| exe_name_by_pid(pid))?;
    let mut class = [0u16; 256];
    // SAFETY: buffer length passed.
    let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    let class = if len > 0 { from_wide(&class[..len as usize]) } else { String::new() };
    Some(AppInfo::from_path(&path, &class))
}

fn image_path(pid: u32) -> Option<String> {
    // SAFETY: handle is checked and closed; buffer length passed in `len`.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            return None;
        }
        Some(from_wide(&buf[..len as usize]))
    }
}

/// Executable names of programs with visible top-level windows (excluding this program), sorted.
pub fn windowed_programs() -> Vec<String> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the Vec passed below, alive for the duration of EnumWindows.
        unsafe {
            if IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER).is_null() && GetWindowTextLengthW(hwnd) > 0 {
                (*(lparam as *mut Vec<HWND>)).push(hwnd);
            }
        }
        1
    }
    // SAFETY: callback only touches the Vec, which outlives the call.
    unsafe { EnumWindows(Some(collect), &mut hwnds as *mut Vec<HWND> as LPARAM) };

    // SAFETY: plain query.
    let own = unsafe { GetCurrentProcessId() };
    let mut names: Vec<String> = Vec::new();
    for hwnd in hwnds {
        let mut pid = 0u32;
        // SAFETY: valid out-pointer.
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == own || pid == 0 {
            continue;
        }
        if let Some(path) = image_path(pid).or_else(|| exe_name_by_pid(pid)) {
            let exe = path.rsplit('\\').next().unwrap_or(&path).to_string();
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&exe)) {
                names.push(exe);
            }
        }
    }
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Calls `f(pid, exe_name)` for every process until it returns true. Returns whether it did.
fn find_process(mut f: impl FnMut(u32, &str) -> bool) -> bool {
    // SAFETY: snapshot handle is checked and closed; entry size is initialised.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            if f(entry.th32ProcessID, &from_wide(&entry.szExeFile)) {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
        found
    }
}

fn exe_name_by_pid(pid: u32) -> Option<String> {
    let mut name = None;
    find_process(|p, exe| {
        if p == pid {
            name = Some(exe.to_string());
            true
        } else {
            false
        }
    });
    name
}

/// Whether a process with this executable name is running.
pub fn is_running(exe: &str) -> bool {
    find_process(|_, name| name.eq_ignore_ascii_case(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_own_process_by_name() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        assert!(is_running(&name), "{name}");
        assert!(is_running(&name.to_uppercase()));
        assert!(!is_running("no-such-process-capslangchange.exe"));
    }
}
