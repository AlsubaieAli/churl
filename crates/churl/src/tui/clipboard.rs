//! Clipboard writes via the OSC 52 terminal escape sequence.
//!
//! **Why OSC 52 and not a native clipboard crate** (DECISIONS.md): zero native
//! dependencies (`arboard` pulls `objc`/`x11`), it works over SSH and inside
//! tmux, and it matches the route lazygit takes. `base64` is already a
//! dependency (M5 basic auth), so the escape framing costs nothing new.
//!
//! **Caveat**: macOS Terminal.app ignores OSC 52 (the payload is silently
//! dropped). iTerm2, kitty, Ghostty, WezTerm, and tmux all support it. This is a
//! terminal capability we cannot detect, so the copy always *reports* success —
//! the message row says "copied" regardless.
//!
//! Terminals may drop very large OSC 52 payloads; the caller caps the copied
//! text before it reaches here (see [`MAX_COPY_BYTES`]).

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Hard cap on the bytes copied in one OSC 52 write. Payloads above a few MB are
/// dropped by some terminals; 1 MB is a safe, generous ceiling. The caller
/// truncates the source text to this many bytes and tells the user.
pub const MAX_COPY_BYTES: usize = 1024 * 1024;

/// Writes `payload` to the terminal clipboard as an OSC 52 sequence
/// (`ESC ] 52 ; c ; <base64> BEL`) and flushes. `out` is the terminal's backend
/// writer (ratatui's `CrosstermBackend`, which is itself a `Write`).
pub fn copy_osc52(payload: &str, out: &mut impl Write) -> std::io::Result<()> {
    let encoded = STANDARD.encode(payload.as_bytes());
    // ESC ] 52 ; c ; <base64> BEL
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_osc52_with_base64_payload() {
        let mut buf: Vec<u8> = Vec::new();
        copy_osc52("hi", &mut buf).unwrap();
        // "hi" base64-encodes to "aGk=".
        assert_eq!(buf, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn empty_payload_still_frames() {
        let mut buf: Vec<u8> = Vec::new();
        copy_osc52("", &mut buf).unwrap();
        assert_eq!(buf, b"\x1b]52;c;\x07");
    }

    #[test]
    fn utf8_payload_encodes_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        copy_osc52("café", &mut buf).unwrap();
        let expected = format!("\x1b]52;c;{}\x07", STANDARD.encode("café".as_bytes()));
        assert_eq!(buf, expected.as_bytes());
    }
}
