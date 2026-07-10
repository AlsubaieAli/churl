//! Layered clipboard writes: native OS clipboard first, OSC 52 (with
//! multiplexer passthrough) as the fallback.
//!
//! **Strategy** (DECISIONS.md — reverses the original "OSC 52, no native dep"):
//!
//! 1. **Native first** — [`arboard`] talks to the real OS clipboard (macOS
//!    NSPasteboard, Windows, Linux **X11**). This is the only path that reliably
//!    reaches the system clipboard across terminals and multiplexers. Native
//!    Wayland is NOT enabled here (arboard's `wayland-data-control` feature is
//!    off), so a pure-Wayland session (no XWayland) uses the OSC 52 fallback.
//!    On Linux the X11 selection is owned by the running process and is cleared
//!    when it exits; churl is a long-lived TUI, so a copy survives for the whole
//!    session — acceptable, and we do not spawn a persistence daemon.
//! 2. **OSC 52 fallback** — when native is unavailable/fails, emit
//!    `ESC ] 52 ; c ; <base64> BEL` to the terminal, **wrapped for the active
//!    multiplexer** so it actually reaches the outer terminal:
//!    - **tmux** (`$TMUX`): DCS passthrough `ESC P tmux ; <osc52> ESC \` with
//!      every `ESC` inside the payload doubled (so the OSC 52's leading ESC
//!      becomes `ESC ESC` — two, not three).
//!    - **GNU screen** (`$STY`, or `$TERM` starting `screen`): `ESC P <osc52>
//!      ESC \`. (screen splits DCS strings at ~768 bytes; our payloads are
//!      normally far smaller and we do not chunk.)
//!    - otherwise: the raw OSC 52 sequence.
//!
//! **Honesty**: native success is known immediately. OSC 52 is fire-and-forget
//! (the terminal never acknowledges), so a successful *write* is reported as a
//! best-effort success — but a total failure (no native clipboard *and* the
//! terminal write erroring) is reported as a failure, never silently swallowed.
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

/// Caps `text` to at most [`MAX_COPY_BYTES`], truncating on a UTF-8 char
/// boundary. Returns the (possibly borrowed-then-owned) payload and its byte
/// length. Pure — the caller reports the length and stores the payload.
pub fn cap_payload(text: &str) -> (String, usize) {
    let payload = if text.len() > MAX_COPY_BYTES {
        let mut end = MAX_COPY_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_owned()
    } else {
        text.to_owned()
    };
    let len = payload.len();
    (payload, len)
}

/// Outcome of a layered copy attempt, so callers can report the truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// Written to the real OS clipboard via [`arboard`].
    Native,
    /// Written as an OSC 52 escape to the terminal (best-effort; the terminal
    /// does not acknowledge, so this is "plausibly copied").
    Osc52,
    /// No path succeeded — nothing reached any clipboard.
    Failed,
}

impl CopyOutcome {
    /// Whether *some* path plausibly placed the text on a clipboard.
    pub fn succeeded(self) -> bool {
        !matches!(self, CopyOutcome::Failed)
    }
}

/// Copies `payload` using the layered strategy: native first, OSC 52 (with
/// multiplexer passthrough) as the fallback.
///
/// `out` is the terminal backend writer (ratatui's `CrosstermBackend`, itself a
/// `Write`) — only touched when the OSC 52 fallback runs. Native copy needs no
/// terminal handle.
pub fn copy(payload: &str, out: &mut impl Write) -> CopyOutcome {
    if copy_native(payload) {
        return CopyOutcome::Native;
    }
    match copy_osc52(payload, out) {
        Ok(()) => CopyOutcome::Osc52,
        Err(_) => CopyOutcome::Failed,
    }
}

/// Attempts a native OS-clipboard write via [`arboard`]. Never panics; any error
/// (no display, no clipboard server, etc.) is treated as "unavailable" so the
/// caller falls back to OSC 52. Not exercised by `cargo test` (headless CI has
/// no clipboard); the unit tests only cover the pure framing logic below.
fn copy_native(payload: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => clipboard.set_text(payload).is_ok(),
        Err(_) => false,
    }
}

/// Which multiplexer, if any, wraps our terminal — decides OSC 52 framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Multiplexer {
    Tmux,
    Screen,
    None,
}

/// Detects the active multiplexer from the environment (`$TMUX`, `$STY`,
/// `$TERM`). Pure over its inputs so it is unit-testable without env mutation.
fn detect_multiplexer(tmux: Option<&str>, sty: Option<&str>, term: Option<&str>) -> Multiplexer {
    if tmux.is_some_and(|v| !v.is_empty()) {
        return Multiplexer::Tmux;
    }
    let in_screen =
        sty.is_some_and(|v| !v.is_empty()) || term.is_some_and(|v| v.starts_with("screen"));
    if in_screen {
        return Multiplexer::Screen;
    }
    Multiplexer::None
}

/// Reads the environment and returns the active multiplexer.
fn current_multiplexer() -> Multiplexer {
    detect_multiplexer(
        std::env::var("TMUX").ok().as_deref(),
        std::env::var("STY").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Builds the raw OSC 52 sequence for `payload`: `ESC ] 52 ; c ; <base64> BEL`.
fn osc52_sequence(payload: &str) -> String {
    let encoded = STANDARD.encode(payload.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

/// Wraps a raw OSC 52 sequence in tmux's DCS passthrough so it reaches the outer
/// terminal. See the inline note for the ESC-doubling rule (load-bearing).
fn wrap_tmux(osc52: &str) -> String {
    // tmux DCS passthrough is `ESC P tmux ; <data> ESC \`, where every ESC inside
    // `<data>` is doubled so it survives tmux's un-doubling (tmux turns each
    // `ESC ESC` back into a single `ESC` before forwarding to the outer
    // terminal). The OSC 52's single leading ESC doubled yields `ESC ESC ]52…`.
    // (No extra prefix ESC — that would emit a stray lone ESC after un-doubling.)
    let doubled = osc52.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{doubled}\x1b\\")
}

/// Wraps a raw OSC 52 sequence in GNU screen's DCS passthrough: `ESC P <osc52>
/// ESC \`. (screen splits DCS strings around 768 bytes; our capped payloads are
/// normally far below that, so we do not chunk.)
fn wrap_screen(osc52: &str) -> String {
    format!("\x1bP{osc52}\x1b\\")
}

/// Builds the OSC 52 escape for `payload`, wrapped for the active multiplexer.
fn framed_osc52(payload: &str, mux: Multiplexer) -> String {
    let osc52 = osc52_sequence(payload);
    match mux {
        Multiplexer::Tmux => wrap_tmux(&osc52),
        Multiplexer::Screen => wrap_screen(&osc52),
        Multiplexer::None => osc52,
    }
}

/// Writes `payload` to the terminal clipboard as an OSC 52 sequence, wrapped for
/// the active multiplexer (tmux / GNU screen passthrough), then flushes. `out`
/// is the terminal's backend writer.
pub fn copy_osc52(payload: &str, out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(framed_osc52(payload, current_multiplexer()).as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_osc52_with_base64_payload() {
        // "hi" base64-encodes to "aGk=".
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn empty_payload_still_frames() {
        assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
    }

    #[test]
    fn utf8_payload_encodes_bytes() {
        let expected = format!("\x1b]52;c;{}\x07", STANDARD.encode("café".as_bytes()));
        assert_eq!(osc52_sequence("café"), expected);
    }

    #[test]
    fn raw_osc52_writes_unwrapped_when_no_multiplexer() {
        let mut buf: Vec<u8> = Vec::new();
        // Directly exercise the writer with an explicit "no mux" frame.
        buf.write_all(framed_osc52("hi", Multiplexer::None).as_bytes())
            .unwrap();
        assert_eq!(buf, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn tmux_wrap_doubles_escapes_and_adds_passthrough() {
        // Inner OSC 52 for "hi" is ESC ] 52 ; c ; aGk= BEL; its single leading
        // ESC is doubled and the whole thing wrapped as ESC P tmux ; <data> ESC \
        // — so `ESC P tmux ;` then `ESC ESC ]52…` (two ESCs, not three).
        assert_eq!(
            framed_osc52("hi", Multiplexer::Tmux),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }

    #[test]
    fn screen_wrap_adds_dcs_passthrough_without_doubling() {
        // screen wraps as ESC P <osc52> ESC \ — no ESC doubling.
        assert_eq!(
            framed_osc52("hi", Multiplexer::Screen),
            "\x1bP\x1b]52;c;aGk=\x07\x1b\\"
        );
    }

    #[test]
    fn detect_multiplexer_prefers_tmux() {
        assert_eq!(
            detect_multiplexer(Some("/tmp/tmux-1000/default,123,0"), None, Some("screen")),
            Multiplexer::Tmux
        );
    }

    #[test]
    fn detect_multiplexer_screen_via_sty() {
        assert_eq!(
            detect_multiplexer(None, Some("1234.pts-0.host"), Some("xterm")),
            Multiplexer::Screen
        );
    }

    #[test]
    fn detect_multiplexer_screen_via_term_prefix() {
        assert_eq!(
            detect_multiplexer(None, None, Some("screen.xterm-256color")),
            Multiplexer::Screen
        );
    }

    #[test]
    fn detect_multiplexer_none_when_bare_terminal() {
        assert_eq!(
            detect_multiplexer(None, None, Some("xterm-256color")),
            Multiplexer::None
        );
    }

    #[test]
    fn detect_multiplexer_ignores_empty_vars() {
        assert_eq!(
            detect_multiplexer(Some(""), Some(""), Some("")),
            Multiplexer::None
        );
    }

    #[test]
    fn copy_outcome_success_flags() {
        assert!(CopyOutcome::Native.succeeded());
        assert!(CopyOutcome::Osc52.succeeded());
        assert!(!CopyOutcome::Failed.succeeded());
    }
}
