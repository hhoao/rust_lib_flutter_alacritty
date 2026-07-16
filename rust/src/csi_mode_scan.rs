//! Incremental sniffer for DEC private modes the vendored alacritty parser
//! does not surface to us:
//!
//! | Mode | Why sniff |
//! |------|-----------|
//! | **2031** | Color-scheme notifications — pinned alacritty predates / ignores it |
//! | **2026** | Synchronized output (`SyncUpdate`) — parsed as a **no-op** (no `TermMode` bit) |
//!
//! ## Why a parallel scanner
//!
//! alacritty's vte parser either swallows unrecognized private modes or, for
//! SyncUpdate, handles them with an empty body and exposes no `EventListener`
//! hook / mode flag. We sniff the raw PTY byte stream in parallel with
//! `parser.advance`.
//!
//! The scanner is **observe-only**: it never removes or rewrites bytes (the same
//! bytes still flow to alacritty), and it carries partial state across
//! [`CsiModeScanner::feed`] calls so a sequence split across two PTY reads —
//! `ESC [ ?` in one chunk, `2026 h` in the next — is still recognized.
//!
//! ## Recognized sequences
//!
//! | Sequence | Event |
//! |----------|--------|
//! | `CSI ? 2031 h` | [`CsiModeEvent::ColorSchemeSubscribe`] |
//! | `CSI ? 2031 l` | [`CsiModeEvent::ColorSchemeUnsubscribe`] |
//! | `CSI ? 2026 h` | [`CsiModeEvent::SyncUpdateSet`] |
//! | `CSI ? 2026 l` | [`CsiModeEvent::SyncUpdateReset`] |
//!
//! Modes may appear among other `;`-separated params (e.g. `CSI ? 1049;2026 h`).

/// DEC private mode number for color-scheme update notifications.
const COLOR_SCHEME_MODE: &str = "2031";

/// DEC private mode number for synchronized output (CSI SyncUpdate).
const SYNC_UPDATE_MODE: &str = "2026";

/// Cap on accumulated CSI parameter bytes. Real mode sequences are short, so a
/// runaway means we mis-synced on binary data — abort rather than grow forever.
const MAX_PARAMS_LEN: usize = 64;

/// A recognized DEC private-mode toggle from the parallel CSI sniffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsiModeEvent {
    /// `CSI ? 2031 h` — application wants color-scheme change notifications.
    ColorSchemeSubscribe,
    /// `CSI ? 2031 l` — application no longer wants them.
    ColorSchemeUnsubscribe,
    /// `CSI ? 2026 h` — synchronized output / present-hold begin.
    SyncUpdateSet,
    /// `CSI ? 2026 l` — synchronized output end; safe to present.
    SyncUpdateReset,
}

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

/// Stateful, observe-only sniffer for modes 2031 and 2026. Construct once per
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

    /// Feed one chunk of raw PTY output. Returns any recognized toggles found, in
    /// stream order. The bytes are neither consumed nor modified — the caller
    /// still hands the identical bytes to the real parser.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<CsiModeEvent> {
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
                        let set = b == b'h';
                        for p in self.params.split(';') {
                            if p == COLOR_SCHEME_MODE {
                                out.push(if set {
                                    CsiModeEvent::ColorSchemeSubscribe
                                } else {
                                    CsiModeEvent::ColorSchemeUnsubscribe
                                });
                            } else if p == SYNC_UPDATE_MODE {
                                out.push(if set {
                                    CsiModeEvent::SyncUpdateSet
                                } else {
                                    CsiModeEvent::SyncUpdateReset
                                });
                            }
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

    fn reset(&mut self) {
        self.state = State::Ground;
        self.params.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CsiModeEvent::*;

    fn scan(bytes: &[u8]) -> Vec<CsiModeEvent> {
        CsiModeScanner::new().feed(bytes)
    }

    #[test]
    fn subscribe_sequence() {
        assert_eq!(scan(b"\x1b[?2031h"), vec![ColorSchemeSubscribe]);
    }

    #[test]
    fn unsubscribe_sequence() {
        assert_eq!(scan(b"\x1b[?2031l"), vec![ColorSchemeUnsubscribe]);
    }

    #[test]
    fn sync_update_set_and_reset() {
        assert_eq!(scan(b"\x1b[?2026h"), vec![SyncUpdateSet]);
        assert_eq!(scan(b"\x1b[?2026l"), vec![SyncUpdateReset]);
    }

    #[test]
    fn surrounded_by_text() {
        assert_eq!(scan(b"before\x1b[?2031hafter"), vec![ColorSchemeSubscribe]);
        assert_eq!(scan(b"before\x1b[?2026hafter"), vec![SyncUpdateSet]);
    }

    #[test]
    fn ignores_other_private_modes() {
        // Bracketed paste + alt screen must not be mistaken for 2031/2026.
        assert!(scan(b"\x1b[?2004h\x1b[?1049h").is_empty());
    }

    #[test]
    fn mode_among_other_params() {
        assert_eq!(scan(b"\x1b[?1049;2031h"), vec![ColorSchemeSubscribe]);
        assert_eq!(scan(b"\x1b[?2031;1004l"), vec![ColorSchemeUnsubscribe]);
        assert_eq!(scan(b"\x1b[?1049;2026h"), vec![SyncUpdateSet]);
        assert_eq!(scan(b"\x1b[?2026;1004l"), vec![SyncUpdateReset]);
    }

    #[test]
    fn both_modes_in_one_sequence() {
        assert_eq!(
            scan(b"\x1b[?2026;2031h"),
            vec![SyncUpdateSet, ColorSchemeSubscribe]
        );
    }

    #[test]
    fn substring_param_does_not_match() {
        assert!(scan(b"\x1b[?12031h").is_empty());
        assert!(scan(b"\x1b[?20310h").is_empty());
        assert!(scan(b"\x1b[?12026h").is_empty());
        assert!(scan(b"\x1b[?20260h").is_empty());
    }

    #[test]
    fn split_across_feeds() {
        let mut s = CsiModeScanner::new();
        assert!(s.feed(b"\x1b[?20").is_empty());
        assert!(s.feed(b"31").is_empty());
        assert_eq!(s.feed(b"h"), vec![ColorSchemeSubscribe]);

        let mut s2 = CsiModeScanner::new();
        assert!(s2.feed(b"\x1b[?20").is_empty());
        assert!(s2.feed(b"26").is_empty());
        assert_eq!(s2.feed(b"h"), vec![SyncUpdateSet]);
    }

    #[test]
    fn split_at_introducer() {
        let mut s = CsiModeScanner::new();
        assert!(s.feed(b"\x1b").is_empty());
        assert!(s.feed(b"[?2031").is_empty());
        assert_eq!(s.feed(b"l"), vec![ColorSchemeUnsubscribe]);
    }

    #[test]
    fn aborted_by_new_escape() {
        assert_eq!(scan(b"\x1b[?20\x1b[?2031h"), vec![ColorSchemeSubscribe]);
        assert_eq!(scan(b"\x1b[?20\x1b[?2026h"), vec![SyncUpdateSet]);
    }

    #[test]
    fn multiple_toggles_in_order() {
        assert_eq!(
            scan(b"\x1b[?2031h\x1b[?2031l\x1b[?2031h"),
            vec![
                ColorSchemeSubscribe,
                ColorSchemeUnsubscribe,
                ColorSchemeSubscribe
            ]
        );
        assert_eq!(
            scan(b"\x1b[?2026h\x1b[?2026l\x1b[?2026h"),
            vec![SyncUpdateSet, SyncUpdateReset, SyncUpdateSet]
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
        assert!(scan(b"\x1b[2031h\x1b[31m\x1b[2026h").is_empty());
    }
}
