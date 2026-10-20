//! Path sanitization for log messages and status files.
//!
//! Linux filenames allow every byte except NUL and `/`. `Path::display()`
//! lossy-escapes only invalid UTF-8 — ESC, CR, LF, backspace, and other
//! control bytes pass through. An attacker who can create a file in a
//! scanned root (`/tmp/`, `/home/<u>/`) can embed terminal escape sequences
//! or newlines in the name; those bytes then land verbatim in the systemd
//! journal, `progress.json`, and anything else that formats a path. Journal
//! viewers and admin terminals rendering `rfind status` will interpret
//! them: `\x1b[2K` erases prior output, `\r` splits lines in SIEM parsers,
//! `\n` fakes new log entries.
//!
//! `sanitize_path` hex-escapes any byte outside the safe ASCII subset
//! (`\x20..=\x7E` minus `\\`), and escapes `\\` itself so the output is
//! unambiguous. Callers wrap it around any path that reaches a terminal,
//! a log sink, or a world-readable file.

use std::path::Path;

/// Hex-escape every byte in `p` that isn't a printable ASCII character
/// (except backslash, which is doubled). The result is always valid UTF-8
/// and safe to render on a terminal, in JSON, or in a syslog line.
pub fn sanitize_path(p: &Path) -> String {
    sanitize_bytes(p.as_os_str().as_encoded_bytes())
}

/// Byte-level variant for the handful of call sites that already hold the
/// raw bytes (e.g. inotify / fanotify event buffers).
pub fn sanitize_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn passes_through_normal_ascii() {
        assert_eq!(sanitize_path(&PathBuf::from("/etc/passwd")), "/etc/passwd");
    }

    #[test]
    fn escapes_control_bytes() {
        // ESC + CSI 2K (ANSI "erase line") + newline — the classic
        // journal-forgery payload.
        let s = sanitize_bytes(b"/tmp/\x1b[2Kpwned\n");
        assert_eq!(s, "/tmp/\\x1b[2Kpwned\\x0a");
    }

    #[test]
    fn escapes_cr_lf_and_backspace() {
        let s = sanitize_bytes(b"a\rb\nc\x08");
        assert_eq!(s, "a\\x0db\\x0ac\\x08");
    }

    #[test]
    fn doubles_backslash() {
        assert_eq!(sanitize_bytes(b"a\\b"), "a\\\\b");
    }

    #[test]
    fn escapes_non_utf8_bytes() {
        // Lone 0xff — valid in a filename, not valid UTF-8.
        let s = sanitize_bytes(b"/weird\xff/x");
        assert_eq!(s, "/weird\\xff/x");
    }
}
