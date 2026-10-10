//! Start with Windows: a per-user Run registry value (normal) or a scheduled task with highest
//! privileges (elevated, needed to handle keys in administrator windows).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenUser,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::log;
use crate::util::{from_wide, wide};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_KEY: &str = r"Software\CapsLangChange";
const VALUE_NAME: &str = "CapsLangChange";
pub const TASK_NAME: &str = "CapsLangChange";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    Normal,
    Elevated,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Normal => "normal",
            Mode::Elevated => "elevated",
        }
    }
}

/// The configured mode, as remembered in `HKCU\Software\CapsLangChange`.
pub fn current_mode() -> Mode {
    match read_string(APP_KEY, "Autostart").as_deref() {
        Some("elevated") => Mode::Elevated,
        Some("normal") => Mode::Normal,
        Some(_) => Mode::Off,
        // No marker yet: infer from the Run value.
        None if read_string(RUN_KEY, VALUE_NAME).is_some() => Mode::Normal,
        None => Mode::Off,
    }
}

pub fn remember_mode(mode: Mode) {
    if let Err(e) = write_string(APP_KEY, "Autostart", mode.as_str()) {
        log::warning!("autostart", "cannot store the autostart mode: {e}");
    }
}

fn run_command(exe: &Path) -> String {
    format!("\"{}\" --autostart", exe.display())
}

pub fn enable_run_value(exe: &Path) -> std::io::Result<()> {
    write_string(RUN_KEY, VALUE_NAME, &run_command(exe))
}

pub fn disable_run_value() {
    delete_value(RUN_KEY, VALUE_NAME);
}

/// Keeps the Run value pointing at the current executable (handles a moved or renamed exe).
pub fn refresh_run_value(exe: &Path) {
    if current_mode() == Mode::Normal {
        let wanted = run_command(exe);
        if read_string(RUN_KEY, VALUE_NAME).as_deref() != Some(wanted.as_str()) {
            match enable_run_value(exe) {
                Ok(()) => log::info!("autostart", "Run value updated to the current executable path"),
                Err(e) => log::warning!("autostart", "cannot update the Run value: {e}"),
            }
        }
    }
}

/// Generic flag: has a one-time notice been shown?
pub fn flag(name: &str) -> bool {
    read_string(APP_KEY, name).as_deref() == Some("1")
}

pub fn set_flag(name: &str) {
    let _ = write_string(APP_KEY, name, "1");
}

// ---------------------------------------------------------------------------------------------
// Scheduled task (requires administrator rights to create or delete).

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A user SID in string form (`S-1-5-21-...`); anything else is rejected.
pub fn valid_sid(sid: &str) -> bool {
    sid.starts_with("S-1-") && sid.len() < 200 && sid[2..].bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

pub fn task_xml(exe: &Path, sid: &str) -> String {
    let exe = xml_escape(&exe.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts CapsLangChange with administrator rights at logon.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--autostart</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn schtasks(args: &[&str]) -> std::io::Result<bool> {
    let status = Command::new("schtasks.exe").args(args).creation_flags(CREATE_NO_WINDOW).status()?;
    Ok(status.success())
}

/// Creates (or replaces) the logon task and optionally starts it now. Must run elevated.
pub fn register_task(exe: &Path, sid: &str, start: bool) -> Result<(), String> {
    if !valid_sid(sid) {
        return Err("invalid user SID".into());
    }
    let xml = task_xml(exe, sid);
    let tmp = std::env::temp_dir().join(format!("CapsLangChange-task-{}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE]; // UTF-16 LE BOM
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write the task definition: {e}"))?;
    let tmp_str = tmp.display().to_string();
    let created = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &tmp_str, "/F"]);
    let _ = std::fs::remove_file(&tmp);
    match created {
        Ok(true) => {}
        Ok(false) => return Err("schtasks could not create the task".into()),
        Err(e) => return Err(format!("cannot run schtasks: {e}")),
    }
    if !start {
        return Ok(());
    }
    match schtasks(&["/Run", "/TN", TASK_NAME]) {
        Ok(true) => Ok(()),
        _ => Err("the task was created but could not be started".into()),
    }
}

/// Deletes the logon task. Must run elevated. A missing task counts as success.
pub fn unregister_task() -> Result<(), String> {
    if !task_exists() {
        return Ok(());
    }
    match schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]) {
        Ok(true) => Ok(()),
        Ok(false) => Err("schtasks could not delete the task".into()),
        Err(e) => Err(format!("cannot run schtasks: {e}")),
    }
}

pub fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME]).unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// Process token helpers.

fn with_token<T>(f: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    // SAFETY: the token handle is checked and closed.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let result = f(token);
        CloseHandle(token);
        result
    }
}

pub fn is_elevated() -> bool {
    with_token(|token| {
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        // SAFETY: buffer of the documented type and size.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
        };
        (ok != 0).then_some(elevation.TokenIsElevated != 0)
    })
    .unwrap_or(false)
}

/// The current user's SID in string form.
pub fn current_user_sid() -> Option<String> {
    with_token(|token| {
        let mut buf = vec![0u8; 256];
        let mut size = 0u32;
        // SAFETY: buffer size passed; the SID string is freed with LocalFree.
        unsafe {
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), buf.len() as u32, &mut size) == 0 {
                return None;
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return None;
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Some(sid)
        }
    })
}

/// True if the executable lives inside the user's profile, where unelevated programs can replace it.
pub fn exe_in_user_profile(exe: &Path) -> bool {
    let Some(profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) else { return false };
    let exe = exe.display().to_string().to_lowercase();
    let profile = profile.display().to_string().to_lowercase();
    !profile.is_empty() && exe.starts_with(&profile)
}

// ---------------------------------------------------------------------------------------------
// Registry helpers (HKCU).

fn read_string(key: &str, name: &str) -> Option<String> {
    let mut buf = [0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer size passed in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(key).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (rc == ERROR_SUCCESS).then(|| from_wide(&buf))
}

fn write_string(key: &str, name: &str, value: &str) -> std::io::Result<()> {
    // SAFETY: handle checked and closed; data pointer/length describe the UTF-16 buffer incl. terminator.
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(key).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_READ,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        let data = wide(value);
        let rc = RegSetValueExW(hkey, wide(name).as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32);
        RegCloseKey(hkey);
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
    }
    Ok(())
}

fn delete_value(key: &str, name: &str) {
    // SAFETY: handle checked and closed.
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(key).as_ptr(), 0, KEY_SET_VALUE, &mut hkey) == ERROR_SUCCESS {
            RegDeleteValueW(hkey, wide(name).as_ptr());
            RegCloseKey(hkey);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_validation() {
        assert!(valid_sid("S-1-5-21-1004336348-1177238915-682003330-512"));
        assert!(!valid_sid("S-1-5-21\"><x"));
        assert!(!valid_sid("Administrator"));
    }

    #[test]
    fn task_xml_escapes_path() {
        let xml = task_xml(Path::new(r"C:\Tools & Co\<CapsLangChange>.exe"), "S-1-5-21-1");
        assert!(xml.contains(r"<Command>C:\Tools &amp; Co\&lt;CapsLangChange&gt;.exe</Command>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<Priority>4</Priority>"));
    }
}
