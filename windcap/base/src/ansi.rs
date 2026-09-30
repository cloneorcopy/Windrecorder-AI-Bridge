//! Decoding bytes that a Win32 console child process wrote to a pipe.
//!
//! Every external tool the recorder shells out to — the OCR engine above all — is a console
//! program whose text output is encoded in whatever code page the console was told to use, and
//! the code page chosen depends on whether stdout is a terminal or a pipe. Getting this wrong is
//! silent and catastrophic: the OCR text *is* the product, and a wrong decode stores rows that
//! read as mojibake in the UI while looking like plausible garbage characters in a log file.
//!
//! So the decode is decided by evidence, not by assumption: candidate code pages are tried in a
//! strict mode that rejects invalid byte sequences, and the first one that consumes the whole
//! buffer without producing a replacement character wins.

/// Decode bytes from a console child process, guessing the code page from validity rather than
/// from a hardcoded assumption about which one Windows is configured for.
///
/// Order matters. UTF-8 first: a .NET child that inherits `DOTNET_SYSTEM_CONSOLE_ALLOW_ANSI_COLOR_REDIRECTION`
/// or runs with the UTF-8 experimental code page enabled emits UTF-8 for every language. Then the
/// console output code page, then the OEM page, then the ANSI page — the last three are the ones
/// a legacy console program lands on when its stdout is redirected.
pub fn decode_console_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // ASCII is valid in every candidate below, and it is the common case for a Western screen.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    for codepage in candidates() {
        if let Some(text) = multibyte_to_utf16(bytes, codepage) {
            if !text.chars().any(|c| c == '\u{fffd}') {
                return text;
            }
        }
    }
    // Nothing consumed the buffer cleanly. Hand back *something* readable; a lossy decode beats
    // dropping a frame's text, and the recorder's own similarity gate will treat the row as noise.
    String::from_utf8_lossy(bytes).into_owned()
}

/// The code pages worth trying, de-duplicated and capped: an explicit UTF-8 probe, the three
/// system pages, and Simplified Chinese, which is what the shipped OCR language default produces
/// on a machine whose ACP is something else entirely.
fn candidates() -> Vec<u32> {
    let mut out = vec![65_001, unsafe { GetConsoleOutputCP() }, unsafe { GetOEMCP() }, unsafe { GetACP() }, 936];
    out.sort_unstable();
    out.dedup();
    out.retain(|cp| *cp != 0);
    out
}

/// Strict multi-byte to UTF-16 conversion: `None` when the byte sequence is not valid in `codepage`,
/// rather than silently substituting U+FFFD, which is what lets the caller reject a wrong guess.
fn multibyte_to_utf16(bytes: &[u8], codepage: u32) -> Option<String> {
    const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;
    unsafe {
        let wide_len = MultiByteToWideChar(
            codepage,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if wide_len <= 0 {
            return None;
        }
        let mut wide = vec![0u16; wide_len as usize];
        let written = MultiByteToWideChar(
            codepage,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            wide_len,
        );
        if written <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&wide[..written as usize]))
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetACP() -> u32;
    fn GetOEMCP() -> u32;
    fn GetConsoleOutputCP() -> u32;
    fn MultiByteToWideChar(
        codepage: u32,
        flags: u32,
        src: *const u8,
        cbsrc: i32,
        dst: *mut u16,
        cwdst: i32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 庞加莱复现定理 — the first line of the shipped zh-Hans OCR fixture.
    const GBK_PANLEV: &[u8] = &[0xc5, 0xd3, 0xbc, 0xd3, 0xc0, 0xb3, 0xb8, 0xb4, 0xcf, 0xd6, 0xb6, 0xa8, 0xc0, 0xed];
    const UTF8_PANLEV: &str = "庞加莱复现定理";

    #[test]
    fn plain_ascii_passes_through() {
        assert_eq!(decode_console_bytes(b"ChatGPT\r\n"), "ChatGPT\r\n");
    }

    #[test]
    fn utf8_is_recognised_without_any_codepage_probe() {
        let bytes = UTF8_PANLEV.as_bytes();
        assert_eq!(decode_console_bytes(bytes), UTF8_PANLEV);
    }

    /// The real shape of the bug this module exists for: the OCR engine writes GBK on a zh-CN
    /// machine, and a naive lossy UTF-8 decode turns it into replacement characters.
    #[test]
    fn gbk_bytes_never_come_back_as_replacement_characters() {
        let text = decode_console_bytes(GBK_PANLEV);
        assert!(!text.chars().any(|c| c == '\u{fffd}'), "mojibake: {text:?}");
        assert_eq!(text, UTF8_PANLEV);
    }

    #[test]
    fn empty_output_is_not_an_error() {
        assert_eq!(decode_console_bytes(&[]), "");
    }
}
