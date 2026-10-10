//! Small Win32 helpers shared by the modules.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

/// Null-terminated UTF-16 string for Win32 calls.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Converts a UTF-16 buffer (up to the first null) to a `String`.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Copies `text` into a fixed-size UTF-16 field, truncating and null-terminating it.
pub fn copy_to_fixed(dst: &mut [u16], text: &str) {
    let mut i = 0;
    for c in text.encode_utf16() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = c;
        i += 1;
    }
    if !dst.is_empty() {
        dst[i] = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_round_trip_and_truncation() {
        let w = wide("Caps Ä");
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(from_wide(&w), "Caps Ä");
        let mut fixed = [1u16; 4];
        copy_to_fixed(&mut fixed, "abcdef");
        assert_eq!(from_wide(&fixed), "abc");
    }
}
