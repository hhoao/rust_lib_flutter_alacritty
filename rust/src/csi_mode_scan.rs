//! Incremental sniffer for the one DEC private mode the vendored alacritty
//! parser hides from us: mode **2031**, the color-scheme change-notification
//! protocol (contour/ghostty/iTerm2, a.k.a. "light/dark mode reporting").
//!
//! ## Why a parallel scanner
//!
//! alacritty's vte parser silently swallows private modes it doesn't recognize
//! and exposes no hook for us to observe them, and the revision we pin predates
//! mode 2031. So we sniff the raw PTY byte stream in parallel with the parser.
//!
//! The scanner is **observe-only**: it never removes or rewrites bytes (the same
//! bytes still flow to alacritty, which harmlessly ignores the unknown mode),
//! and it carries partial state across [`CsiModeScanner::feed`] calls so a
//! sequence split across two PTY reads — `ESC [ ?` in one chunk, `2031 h` in the
//! next — is still recognized.
//!
//! ## Recognized sequences
//!
//! | Sequence | Toggle |
//! |----------|--------|
//! | `CSI ? 2031 h` (`ESC [ ? 2031 h`) | [`ColorSchemeToggle::Subscribe`] |
//! | `CSI ? 2031 l` | [`ColorSchemeToggle::Unsubscribe`] |
//!
//! `2031` may appear among other `;`-separated params (e.g. `CSI ? 1049;2031 h`).

/// A recognized toggle of DEC private mode 2031.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSchemeToggle {
    /// `CSI ? 2031 h` — application wants color-scheme change notifications.
    Subscribe,
    /// `CSI ? 2031 l` — application no longer wants them.
    Unsubscribe,
}

/// DEC private mode number for color-scheme update notifications.
const COLOR_SCHEME_MODE: &str = "2031";

/// Cap on accumulated CSI parameter bytes. Real mode sequences are short, so a
/// runaway means we mis-synced on binary data — abort rather than grow forever.
const MAX_PARAMS_LEN: usize = 64;

#[derive(Default, Clone, Copy)]
enum State {
    /// Outside any escape sequence.
    #[default]
    Ground,
    /// Saw `ESC` (0x1b).
    Esc,
    /// Saw `ESC [`.
    Csi,
    /// Saw `ESC [ ?` — accumulating numeric/`;` params until a final byte.
    CsiPriv,
}

/// Stateful, observe-only sniffer for mode-2031 toggles. Construct once per
/// engine and [`feed`](Self::feed) every chunk of raw PTY output.
#[derive(Default)]
pub struct CsiModeScanner {
    state: State,
    params: String,
}

impl CsiModeScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk of raw PTY output. Returns any mode-2031 toggles found, in
    /// stream order. The bytes are neither consumed nor modified — the caller
    /// still hands the identical bytes to the real parser.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ColorSchemeToggle> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    self.state = match b {
                        b'[' => State::Csi,
                        0x1b => State::Esc, // ESC ESC — restart on the new ESC.
                        _ => State::Ground,
                    };
                }
                State::Csi => {
                    self.state = match b {
                        b'?' => {
                            self.params.clear();
                            State::CsiPriv
                        }
                        0x1b => State::Esc,
                        _ => State::Ground, // not a private-mode CSI; stop tracking.
                    };
                }
                State::CsiPriv => match b {
                    b'0'..=b'9' | b';' => {
                        self.params.push(b as char);
                        if self.params.len() > MAX_PARAMS_LEN {
                            self.reset();
                        }
                    }
                    b'h' | b'l' => {
                        if self.has_color_scheme_mode() {
                            out.push(if b == b'h' {
                                ColorSchemeToggle::Subscribe
                            } else {
                                ColorSchemeToggle::Unsubscribe
                            });
                        }
                        self.reset();
                    }
                    0x1b => {
                        // A fresh ESC aborts the in-flight (intermixed) sequence.
                        self.params.clear();
                        self.state = State::Esc;
                    }
                    _ => self.reset(),
                },
            }
        }
        out
    }

    fn has_color_scheme_mode(&self) -> bool {
        self.params.split(';').any(|p| p == COLOR_SCHEME_MODE)
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.params.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ColorSchemeToggle::*;

    fn scan(bytes: &[u8]) -> Vec<ColorSchemeToggle> {
        CsiModeScanner::new().feed(bytes)
    }

    #[test]
    fn subscribe_sequence() {
        assert_eq!(scan(b"\x1b[?2031h"), vec![Subscribe]);
    }

    #[test]
    fn unsubscribe_sequence() {
        assert_eq!(scan(b"\x1b[?2031l"), vec![Unsubscribe]);
    }

    #[test]
    fn surrounded_by_text() {
        assert_eq!(scan(b"before\x1b[?2031hafter"), vec![Subscribe]);
    }

    #[test]
    fn ignores_other_private_modes() {
        // Bracketed paste + alt screen must not be mistaken for 2031.
        assert!(scan(b"\x1b[?2004h\x1b[?1049h").is_empty());
    }

    #[test]
    fn mode_2031_among_other_params() {
        assert_eq!(scan(b"\x1b[?1049;2031h"), vec![Subscribe]);
        assert_eq!(scan(b"\x1b[?2031;1004l"), vec![Unsubscribe]);
    }

    #[test]
    fn substring_param_does_not_match() {
        // "12031" / "20310" must not be read as 2031.
        assert!(scan(b"\x1b[?12031h").is_empty());
        assert!(scan(b"\x1b[?20310h").is_empty());
    }

    #[test]
    fn split_across_feeds() {
        let mut s = CsiModeScanner::new();
        assert!(s.feed(b"\x1b[?20").is_empty());
        assert!(s.feed(b"31").is_empty());
        assert_eq!(s.feed(b"h"), vec![Subscribe]);
    }

    #[test]
    fn split_at_introducer() {
        let mut s = CsiModeScanner::new();
        assert!(s.feed(b"\x1b").is_empty());
        assert!(s.feed(b"[?2031").is_empty());
        assert_eq!(s.feed(b"l"), vec![Unsubscribe]);
    }

    #[test]
    fn aborted_by_new_escape() {
        // ESC mid-params abandons the first attempt, then a clean sequence wins.
        assert_eq!(scan(b"\x1b[?20\x1b[?2031h"), vec![Subscribe]);
    }

    #[test]
    fn multiple_toggles_in_order() {
        assert_eq!(
            scan(b"\x1b[?2031h\x1b[?2031l\x1b[?2031h"),
            vec![Subscribe, Unsubscribe, Subscribe]
        );
    }

    #[test]
    fn runaway_params_abort() {
        let mut input = b"\x1b[?".to_vec();
        input.extend(std::iter::repeat(b'1').take(200));
        input.push(b'h');
        assert!(scan(&input).is_empty());
    }

    #[test]
    fn plain_csi_without_question_mark_ignored() {
        // SGR and a non-private CSI must never enter the params accumulator.
        assert!(scan(b"\x1b[2031h\x1b[31m").is_empty());
    }
}
