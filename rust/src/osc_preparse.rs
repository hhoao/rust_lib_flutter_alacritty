/// Pre-parser that scans incoming PTY bytes for OSC sequences that the vte
/// crate routes to `unhandled()` (OSC 7, 9, 777) and extracts them before the
/// bytes reach the parser.
///
/// ## Supported sequences
///
/// | OSC | Format | Event |
/// |-----|--------|-------|
/// | 7   | `ESC ] 7 ; file://host/path ST` | `EngineEvent::WorkingDir` |
/// | 9   | `ESC ] 9 ; <message> ST` | `EngineEvent::Notify` |
/// | 777 | `ESC ] 777 ; notify; <title>; <body> ST` | `EngineEvent::Notify` ("title\0body") |
///
/// ST (string terminator) is BEL (`0x07`) or `ESC \` (`0x1b 0x5c`).
///
/// ## Design
///
/// We walk the byte slice once, building a filtered output buffer and a list of
/// extracted events. Sequences the vte parser already handles (OSC 0/2/4/8/10/
/// 11/12/22/50/52/104/110/111/112) pass through untouched — we only intercept
/// the ones vte drops.
///
/// Malformed sequences (missing terminator, non-numeric parameter, empty
/// payload) pass through unmodified so vte can log them via its own `debug!`
/// path if desired.

use crate::event_proxy::EngineEvent;

/// Maximum payload length to accept for OSC 7/9/777 (4 KiB). Longer payloads
/// pass through unmodified — they're likely not valid OSC sequences.
const MAX_PAYLOAD: usize = 4096;

/// Maximum parameter length (Ps) — "777" is 3 chars, but cap at 5 to be safe.
const MAX_PARAM: usize = 5;

/// Scan `bytes` for OSC 7/9/777, strip matched sequences, and return extracted
/// events together with the filtered byte buffer.
///
/// ```
/// use rust_lib_flutter_alacritty::osc_preparse::extract_osc_events;
/// use rust_lib_flutter_alacritty::event_proxy::EngineEvent;
///
/// let (filtered, events) = extract_osc_events(
///     b"\x1b]7;file://box/home/proj\x07hello"
/// );
/// assert_eq!(filtered, b"hello");
/// assert_eq!(events.len(), 1);
/// assert!(matches!(&events[0], EngineEvent::WorkingDir(d) if d == "file://box/home/proj"));
/// ```
pub fn extract_osc_events(bytes: &[u8]) -> (Vec<u8>, Vec<EngineEvent>) {
    let mut filtered = Vec::with_capacity(bytes.len());
    let mut events = Vec::new();
    let mut pos: usize = 0;
    let len = bytes.len();

    while pos < len {
        // Look for ESC ] (0x1b 0x5d) — the OSC introducer.
        let osc_start = match find_osc_introducer(bytes, pos) {
            Some(i) => i,
            None => {
                // No more OSC sequences; copy the rest and finish.
                filtered.extend_from_slice(&bytes[pos..]);
                break;
            }
        };

        // Copy bytes between pos and osc_start (non-OSC content).
        if osc_start > pos {
            filtered.extend_from_slice(&bytes[pos..osc_start]);
        }

        // pos now at the ESC byte. Try to parse the full OSC sequence.
        let after_intro = osc_start + 2; // skip ESC ]
        match parse_osc_sequence(bytes, after_intro) {
            OscParseResult::Known { param, payload, end } => {
                match param {
                    OscParam::Seven => events.push(EngineEvent::WorkingDir(payload)),
                    OscParam::Nine => events.push(EngineEvent::Notify(payload)),
                    OscParam::SevenSevenSeven => {
                        // OSC 777: format is "notify; <title>; <body>"
                        // Combine as "title\0body" so the Dart side can split.
                        let combined = format_osc777_notify(&payload);
                        events.push(EngineEvent::Notify(combined));
                    }
                }
                pos = end;
            }
            OscParseResult::Passthrough { end } => {
                // Unknown parameter or malformed — copy original bytes through.
                // `end` points to after the ST, or to the end of the buffer.
                filtered.extend_from_slice(&bytes[osc_start..end]);
                pos = end;
            }
            OscParseResult::Incomplete => {
                // Ran out of bytes looking for ST. Copy ESC ] + what we have
                // and let the next advance() call re-scan if more data arrives.
                filtered.extend_from_slice(&bytes[osc_start..]);
                break;
            }
        }
    }

    (filtered, events)
}

#[derive(Debug, PartialEq)]
enum OscParam {
    Seven,
    Nine,
    SevenSevenSeven,
}

enum OscParseResult {
    /// Recognized OSC (7/9/777) — strip from output, emit event.
    Known { param: OscParam, payload: String, end: usize },
    /// Unknown param or malformed — copy original bytes through to `end`.
    Passthrough { end: usize },
    /// ST not found in remaining buffer — copy everything through.
    Incomplete,
}

fn find_osc_introducer(bytes: &[u8], start: usize) -> Option<usize> {
    bytes[start..]
        .windows(2)
        .position(|w| w == [0x1b, 0x5d])
        .map(|p| start + p)
}

/// Parse an OSC sequence starting just after `ESC ]` (i.e. at the parameter
/// bytes). Returns the parsed result and the byte position just after the ST.
fn parse_osc_sequence(bytes: &[u8], start: usize) -> OscParseResult {
    let len = bytes.len();
    if start >= len {
        return OscParseResult::Incomplete;
    }

    // --- parse parameter (Ps) ------------------------------------------------
    let param_end = match find_semicolon_or_non_digit(bytes, start) {
        Some(i) => i,
        None => {
            // No semicolon found — can't be a valid OSC. Pass through.
            // Include the ESC ] intro + remaining bytes.
            // But we need `end` as the full position after escape attempt.
            // Since no semicolon, this is malformed; pass everything.
            return OscParseResult::Passthrough { end: len };
        }
    };

    // The separator character must be a semicolon; non-digit before semicolon
    // means malformed → passthrough.
    if !is_semicolon_at(bytes, param_end) {
        return OscParseResult::Passthrough { end: len };
    }

    let param_bytes = &bytes[start..param_end];
    if param_bytes.is_empty() || param_bytes.len() > MAX_PARAM {
        return OscParseResult::Passthrough { end: len };
    }

    let param = match parse_param(param_bytes) {
        Some(p) => p,
        None => return OscParseResult::Passthrough { end: len },
    };

    // --- parse payload (Pt) --------------------------------------------------
    // Payload starts after the semicolon.
    let payload_start = param_end + 1;
    if payload_start >= len {
        return OscParseResult::Incomplete;
    }

    // Find the ST (BEL 0x07 or ESC \ 0x1b 0x5c).
    let st_pos = match find_st(&bytes[payload_start..]) {
        Some(rel) => payload_start + rel,
        None => return OscParseResult::Incomplete,
    };

    let payload_raw = &bytes[payload_start..st_pos];
    if payload_raw.len() > MAX_PAYLOAD {
        return OscParseResult::Passthrough { end: st_pos + st_len(bytes[st_pos]) };
    }

    let payload = String::from_utf8_lossy(payload_raw).into_owned();

    // Determine end position (after ST).
    let end = st_pos + st_len(bytes[st_pos]);

    OscParseResult::Known { param, payload, end }
}

fn st_len(byte: u8) -> usize {
    if byte == 0x1b {
        // Must be ESC \ (0x1b 0x5c) — 2 bytes.
        2
    } else {
        // BEL (0x07) — 1 byte.
        1
    }
}

/// Find the first occurrence of the ST terminator (BEL or ESC \) in `bytes`.
fn find_st(bytes: &[u8]) -> Option<usize> {
    for (i, &b) in bytes.iter().enumerate() {
        if b == 0x07 {
            return Some(i);
        }
        if b == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == 0x5c {
            return Some(i);
        }
    }
    None
}

fn find_semicolon_or_non_digit(bytes: &[u8], start: usize) -> Option<usize> {
    for (i, &b) in bytes[start..].iter().enumerate() {
        match b {
            b';' => return Some(start + i),
            b'0'..=b'9' => continue,
            _ => {
                // Non-digit, non-semicolon before semicolon — malformed.
                // Return this position but caller will check it's actually ';'.
                return Some(start + i);
            }
        }
    }
    None
}

/// Check that the byte at `pos` is actually a semicolon. Used after
/// `find_semicolon_or_non_digit` to distinguish valid `Ps;Pt` from
/// malformed `PsX...` (non-digit before semicolon).
fn is_semicolon_at(bytes: &[u8], pos: usize) -> bool {
    pos < bytes.len() && bytes[pos] == b';'
}

fn parse_param(bytes: &[u8]) -> Option<OscParam> {
    match bytes {
        b"7" => Some(OscParam::Seven),
        b"9" => Some(OscParam::Nine),
        b"777" => Some(OscParam::SevenSevenSeven),
        _ => None,
    }
}

/// Format OSC 777 payload. Input: `notify; <title>; <body>` (iTerm2 convention).
/// Output: `"<title>\0<body>"` — zero-delimited so Dart can split cheaply.
/// If the payload doesn't match the `notify;...` convention, return it as-is
/// (bare notification).
fn format_osc777_notify(raw: &str) -> String {
    // Expected: "notify; <title>; <body>"
    let rest = raw.strip_prefix("notify").and_then(|r| r.strip_prefix(|c| c == ';' || c == ' '));
    let rest = match rest {
        Some(r) => r,
        None => return raw.to_string(),
    };

    // Now rest is "<title>; <body>" or just "<title>"
    if let Some(semi) = rest.find(';') {
        let title = rest[..semi].trim();
        let body = rest[semi + 1..].trim();
        format!("{}\0{}", title, body)
    } else {
        rest.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers -----------------------------------------------------------

    fn extract(bytes: &[u8]) -> (Vec<u8>, Vec<EngineEvent>) {
        extract_osc_events(bytes)
    }

    fn working_dir(s: &str) -> EngineEvent {
        EngineEvent::WorkingDir(s.into())
    }

    fn notify(s: &str) -> EngineEvent {
        EngineEvent::Notify(s.into())
    }

    // ---- passthrough (non-intercepted OSCs) --------------------------------

    #[test]
    fn unknown_osc_passthrough() {
        // OSC 0 (title) — vte handles this, we pass it through.
        let input = b"\x1b]0;my title\x07";
        let (f, e) = extract(input);
        assert_eq!(f, input);
        assert!(e.is_empty());
    }

    #[test]
    fn plain_text_passthrough() {
        let input = b"hello world";
        let (f, e) = extract(input);
        assert_eq!(f, b"hello world");
        assert!(e.is_empty());
    }

    #[test]
    fn text_with_esc_not_osc() {
        // ESC alone without ] — not an OSC introducer.
        let input = b"abc\x1b[31mred\x1b[0m";
        let (f, e) = extract(input);
        assert_eq!(f, b"abc\x1b[31mred\x1b[0m");
        assert!(e.is_empty());
    }

    // ---- OSC 7 (cwd) -------------------------------------------------------

    #[test]
    fn osc7_bel_terminated() {
        let input = b"before\x1b]7;file://myhost/home/user\x07after";
        let (f, e) = extract(input);
        assert_eq!(f, b"beforeafter");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], working_dir("file://myhost/home/user"));
    }

    #[test]
    fn osc7_st_terminated() {
        let input = b"before\x1b]7;file://host/path\x1b\\after";
        let (f, e) = extract(input);
        assert_eq!(f, b"beforeafter");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], working_dir("file://host/path"));
    }

    #[test]
    fn osc7_multiple() {
        let input = b"\x1b]7;file://a/dir1\x07middle\x1b]7;file://a/dir2\x07end";
        let (f, e) = extract(input);
        assert_eq!(f, b"middleend");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0], working_dir("file://a/dir1"));
        assert_eq!(e[1], working_dir("file://a/dir2"));
    }

    // ---- OSC 9 (notify) ----------------------------------------------------

    #[test]
    fn osc9_simple() {
        let input = b"\x1b]9;Build complete\x07";
        let (f, e) = extract(input);
        assert_eq!(f, b"");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], notify("Build complete"));
    }

    #[test]
    fn osc9_bel_terminated_with_text() {
        let input = b"run\x1b]9;done\x07next";
        let (f, e) = extract(input);
        assert_eq!(f, b"runnext");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], notify("done"));
    }

    #[test]
    fn osc9_empty_body() {
        let input = b"\x1b]9;\x07";
        let (f, e) = extract(input);
        assert_eq!(f, b"");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], notify(""));
    }

    // ---- OSC 777 (notify with title) ---------------------------------------

    #[test]
    fn osc777_with_title_and_body() {
        let input = b"\x1b]777;notify;CI finished;All tests passed\x07";
        let (f, e) = extract(input);
        assert_eq!(f, b"");
        assert_eq!(e.len(), 1);
        // Combined: title\0body
        assert_eq!(e[0], notify("CI finished\0All tests passed"));
    }

    #[test]
    fn osc777_notify_only_title() {
        let input = b"\x1b]777;notify;Done\x07";
        let (f, e) = extract(input);
        assert_eq!(f, b"");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], notify("Done"));
    }

    #[test]
    fn osc777_no_notify_prefix() {
        let input = b"\x1b]777;raw message here\x07";
        let (f, e) = extract(input);
        assert_eq!(f, b"");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0], notify("raw message here"));
    }

    // ---- edge cases --------------------------------------------------------

    #[test]
    fn malformed_no_semicolon_passthrough() {
        let input = b"\x1b]7file://nope\x07rest";
        let (f, e) = extract(input);
        assert_eq!(f, input);
        assert!(e.is_empty());
    }

    #[test]
    fn malformed_non_numeric_param_passthrough() {
        let input = b"\x1b]abc;payload\x07";
        let (f, e) = extract(input);
        assert_eq!(f, input);
        assert!(e.is_empty());
    }

    #[test]
    fn incomplete_no_terminator() {
        let input = b"prefix\x1b]7;file://incomplete";
        let (f, e) = extract(input);
        // The ESC ] ... part is passed through because we can't be sure it's
        // complete. Next advance() may get the rest.
        assert_eq!(f, input);
        assert!(e.is_empty());
    }

    #[test]
    fn payload_too_long_passthrough() {
        let big = "x".repeat(5000);
        let input = format!("\x1b]7;{}\x07", big);
        let (f, e) = extract(input.as_bytes());
        assert_eq!(f, input.as_bytes());
        assert!(e.is_empty());
    }

    #[test]
    fn mixed_osc_and_regular() {
        let input = b"\x1b]9;notif\x07regular\x1b]7;file://cwd\x07more";
        let (f, e) = extract(input);
        assert_eq!(f, b"regularmore");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0], notify("notif"));
        assert_eq!(e[1], working_dir("file://cwd"));
    }

    #[test]
    fn osc_8_hyperlink_passthrough() {
        // vte handles OSC 8 — we must pass it through.
        let input = b"\x1b]8;id=foo;https://example.com\x07text\x1b]8;;\x07";
        let (f, e) = extract(input);
        assert_eq!(f, input);
        assert!(e.is_empty());
    }

    #[test]
    fn osc_52_clipboard_passthrough() {
        // vte handles OSC 52 — pass through.
        let input = b"\x1b]52;c;dGhpcyBpcyBhIHRlc3Q=\x07";
        let (f, e) = extract(input);
        assert_eq!(f, input);
        assert!(e.is_empty());
    }
}
