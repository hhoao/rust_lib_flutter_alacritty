use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::csi_mode_scan::{ColorSchemeToggle, CsiModeScanner};
use crate::event_proxy::{
    ClipboardReplyQueue, EngineEvent, EventProxy, EventQueue, ReplyQueue, SizeReplyQueue,
};
use crate::osc_preparse::extract_osc_events;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{point_to_viewport, viewport_to_point, Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};

/// Smallest grid the VT model can actually represent.
///
/// `Term::input` writes a fullwidth glyph and then unconditionally advances the
/// cursor one cell to write its trailing spacer (term/mod.rs). With a single
/// column that advance lands past the row end and `Grid::cursor_cell` panics
/// (`index out of bounds: the len is 1 but the index is 1`). A terminal must
/// therefore hold at least one fullwidth cell plus its spacer — alacritty's own
/// window layer enforces the same floor. We pin it at the one boundary where an
/// external `(columns, rows)` becomes a `TermSize`, so no caller (Dart, view
/// layout, tests) can ever construct a degenerate grid.
const MIN_COLUMNS: usize = 2;
const MIN_SCREEN_LINES: usize = 1;

/// The only place a `(columns, rows)` pair is turned into grid geometry. Clamps
/// to the VT minimum so an invalid grid size is unrepresentable past this point.
fn clamped_term_size(columns: u16, rows: u16) -> alacritty_terminal::term::test::TermSize {
    alacritty_terminal::term::test::TermSize::new(
        (columns as usize).max(MIN_COLUMNS),
        (rows as usize).max(MIN_SCREEN_LINES),
    )
}

/// Flat, FFI-friendly cell. fg/bg are packed 0x00RRGGBB.
#[derive(Clone, Debug)]
pub struct CellData {
    pub codepoint: u32,
    pub fg: u32,
    pub bg: u32,
    pub flags: u16,
    pub hyperlink_id: u32,
}

/// Flat, columnar line for the FFI boundary. Storing each attribute in its own
/// primitive vector lets flutter_rust_bridge decode them as Dart typed lists
/// (`Uint32List` / `Uint16List`) in one shot — no per-cell Dart object is
/// allocated, unlike a `Vec<CellData>` which materializes one object per column.
/// Built internally from `Vec<CellData>` (which keeps the ergonomic per-cell
/// engine logic) and packed only at the boundary via [`LineUpdate::from_cells`].
#[derive(Clone, Debug)]
pub struct LineUpdate {
    pub line: u32,
    pub codepoints: Vec<u32>,
    pub fg: Vec<u32>,
    pub bg: Vec<u32>,
    pub flags: Vec<u16>,
    pub hyperlink_id: Vec<u32>,
}

impl LineUpdate {
    /// Packs a row of per-cell [`CellData`] into the columnar FFI layout.
    fn from_cells(line: u32, cells: Vec<CellData>) -> LineUpdate {
        let n = cells.len();
        let mut codepoints = Vec::with_capacity(n);
        let mut fg = Vec::with_capacity(n);
        let mut bg = Vec::with_capacity(n);
        let mut flags = Vec::with_capacity(n);
        let mut hyperlink_id = Vec::with_capacity(n);
        for c in cells {
            codepoints.push(c.codepoint);
            fg.push(c.fg);
            bg.push(c.bg);
            flags.push(c.flags);
            hyperlink_id.push(c.hyperlink_id);
        }
        LineUpdate { line, codepoints, fg, bg, flags, hyperlink_id }
    }

    /// Column count of this line.
    fn len(&self) -> usize {
        self.codepoints.len()
    }

    /// Reconstructs a single cell from the columnar layout (test ergonomics).
    #[cfg(test)]
    fn cell(&self, col: usize) -> CellData {
        CellData {
            codepoint: self.codepoints[col],
            fg: self.fg[col],
            bg: self.bg[col],
            flags: self.flags[col],
            hyperlink_id: self.hyperlink_id[col],
        }
    }
}

/// Internal row being built before it is packed into a columnar [`LineUpdate`].
/// Keeps `cells` so the snapshot/selection/wide-pair logic stays per-cell.
struct RawLine {
    line: u32,
    cells: Vec<CellData>,
}

impl RawLine {
    fn pack(self) -> LineUpdate {
        LineUpdate::from_cells(self.line, self.cells)
    }
}

#[derive(Clone, Debug)]
pub struct RenderUpdate {
    pub lines: Vec<LineUpdate>,
    pub full: bool,
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub cursor_visible: bool,
    pub cursor_shape: u8,
    pub cursor_blinking: bool,
    pub mode_flags: u32,
    pub display_offset: u32,
    pub default_fg: u32,
    pub default_bg: u32,
    pub cursor_color: u32,
    /// Sub-cell scroll position in [0.0, 1.0): how far, in fractions of a cell
    /// height, the viewport is scrolled up past `display_offset`. The painter
    /// shifts content DOWN by `scroll_fraction * cell_height` and fills the
    /// revealed top sliver with the overscan row (the last entry in `lines` on
    /// a full update). 0.0 when sitting on a line boundary.
    pub scroll_fraction: f64,
    /// Net viewport line scroll since the last mirror state. Positive = scrolled
    /// up into history. Zero on full updates and on sub-cell-only pixel scrolls.
    /// The Dart mirror rotates existing rows by this delta before applying
    /// [lines], so scroll refresh only ships the edge rows that entered view.
    pub scroll_line_delta: i32,
}

impl RenderUpdate {
    /// Column count inferred from the first line (test/diagnostic helper).
    pub fn columns(&self) -> usize {
        self.lines.first().map(|l| l.len()).unwrap_or(0)
    }
}

/// Color configuration passed from Dart at engine creation.
/// `palette` is length 18: [0..15] = ANSI colors, [16] = default fg, [17] = default bg
/// (each packed 0x00RRGGBB). `scrollback` = max history lines.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub palette: Vec<u32>,
    pub scrollback: u32,
    pub osc52: u8,
    pub semantic_escape_chars: String,
    pub default_cursor_shape: u8,
    pub default_cursor_blinking: bool,
}

impl EngineConfig {
    /// The canonical v1 palette (ANSI 0-15, then default fg, default bg).
    pub fn default_palette() -> [u32; 18] {
        [
            0x0000_0000, 0x00CC_0000, 0x004E_9A06, 0x00C4_A000, 0x0034_65A4, 0x0075_507B,
            0x0006_989A, 0x00D3_D7CF, 0x0055_5753, 0x00EF_2929, 0x008A_E234, 0x00FC_E94F,
            0x0072_9FCF, 0x00AD_7FA8, 0x0034_E2E2, 0x00EE_EEEC,
            DEFAULT_FG, DEFAULT_BG,
        ]
    }

    pub fn defaults() -> EngineConfig {
        EngineConfig {
            palette: Self::default_palette().to_vec(),
            scrollback: 10000,
            osc52: 1,
            semantic_escape_chars: String::from(",│`|:\"' ()[]{}<>\t"),
            default_cursor_shape: 0,
            default_cursor_blinking: false,
        }
    }
}

fn build_term_config(c: &EngineConfig) -> Config {
    use alacritty_terminal::term::Osc52;
    use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};
    let osc52 = match c.osc52 {
        0 => Osc52::Disabled,
        2 => Osc52::OnlyPaste,
        3 => Osc52::CopyPaste,
        _ => Osc52::OnlyCopy,
    };
    let shape = match c.default_cursor_shape {
        1 => CursorShape::Underline,
        2 => CursorShape::Beam,
        3 => CursorShape::HollowBlock,
        4 => CursorShape::Hidden,
        _ => CursorShape::Block,
    };
    Config {
        scrolling_history: c.scrollback as usize,
        semantic_escape_chars: c.semantic_escape_chars.clone(),
        default_cursor_style: CursorStyle {
            shape,
            blinking: c.default_cursor_blinking,
        },
        osc52,
        ..Default::default()
    }
}

// Bit layout for CellData.flags (a subset for the tracer bullet).
pub const FLAG_BOLD: u16 = 1 << 0;
pub const FLAG_ITALIC: u16 = 1 << 1;
pub const FLAG_UNDERLINE: u16 = 1 << 2;
pub const FLAG_INVERSE: u16 = 1 << 3;
pub const FLAG_WIDE: u16 = 1 << 4;
pub const FLAG_WIDE_SPACER: u16 = 1 << 5;
pub const FLAG_DIM: u16 = 1 << 6;
pub const FLAG_STRIKEOUT: u16 = 1 << 7;
pub const FLAG_SELECTED: u16 = 1 << 8;
/// Sentinel `LineUpdate.line` on incremental scroll refreshes (not full snapshots,
/// which tag overscan with `screen_lines` as the last viewport entry).
pub const OVERSCAN_LINE_TAG: u32 = u32::MAX;
pub const FLAG_MATCH: u16 = 1 << 9;
pub const FLAG_MATCH_CURRENT: u16 = 1 << 10;
pub const FLAG_HYPERLINK: u16 = 1 << 11;

const DEFAULT_FG: u32 = 0x00D8_D8D8;
const DEFAULT_BG: u32 = 0x0018_1818;

/// Fallback cell height (px) used by `scroll_pixels` only before the host has
/// reported real metrics via `set_cell_pixels` — a rough mid-range row height.
const DEFAULT_CELL_HEIGHT_PX: f64 = 16.0;

/// Sentinel `cursor_color` when no program has set OSC 12. Impossible as a
/// real packed color (`pack` always yields `0x00RRGGBB`), so Dart can
/// distinguish "unset → keep inverse-video cursor" from a real cursor color.
pub const CURSOR_COLOR_UNSET: u32 = 0xFF00_0000;

fn pack(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

fn unpack(v: u32) -> Rgb {
    Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }
}

/// Whether a packed `0x00RRGGBB` background reads as "dark" — used to pick the
/// OSC 997 color-scheme report value. Rec. 709 relative luminance, midpoint 128.
fn is_dark_bg(packed: u32) -> bool {
    let r = ((packed >> 16) & 0xFF) as f64;
    let g = ((packed >> 8) & 0xFF) as f64;
    let b = (packed & 0xFF) as f64;
    0.2126 * r + 0.7152 * g + 0.0722 * b < 128.0
}

fn map_flags(f: Flags) -> u16 {
    let mut out = 0u16;
    if f.contains(Flags::BOLD) {
        out |= FLAG_BOLD;
    }
    if f.contains(Flags::ITALIC) {
        out |= FLAG_ITALIC;
    }
    if f.intersects(Flags::ALL_UNDERLINES) {
        out |= FLAG_UNDERLINE;
    }
    if f.contains(Flags::INVERSE) {
        out |= FLAG_INVERSE;
    }
    if f.contains(Flags::WIDE_CHAR) {
        out |= FLAG_WIDE;
    }
    if f.contains(Flags::WIDE_CHAR_SPACER) {
        out |= FLAG_WIDE_SPACER;
    }
    if f.contains(Flags::DIM) {
        out |= FLAG_DIM;
    }
    if f.contains(Flags::STRIKEOUT) {
        out |= FLAG_STRIKEOUT;
    }
    out
}

fn point_in_range(p: Point, r: &SelectionRange) -> bool {
    let after_start = p.line > r.start.line
        || (p.line == r.start.line && p.column >= r.start.column);
    let before_end = p.line < r.end.line
        || (p.line == r.end.line && p.column <= r.end.column);
    after_start && before_end
}

fn point_in_match(p: Point, m: &Match) -> bool {
    let (start, end) = (m.start(), m.end());
    let after_start =
        p.line > start.line || (p.line == start.line && p.column >= start.column);
    let before_end = p.line < end.line || (p.line == end.line && p.column <= end.column);
    after_start && before_end
}

fn sel_type(kind: u8) -> SelectionType {
    match kind {
        1 => SelectionType::Semantic,
        2 => SelectionType::Lines,
        _ => SelectionType::Simple,
    }
}

pub struct TerminalEngine {
    term: Term<EventProxy>,
    parser: Processor,
    events: EventQueue,
    replies: ReplyQueue,
    clipboard: ClipboardReplyQueue,
    sizes: SizeReplyQueue,
    cell_w: u16,
    cell_h: u16,
    palette: [u32; 18],
    search: Option<RegexSearch>,
    current_match: Option<Match>,
    hyperlinks: Vec<String>,
    hyperlink_ids: HashMap<String, u32>,
    /// Sub-cell scroll accumulator in cell-height fractions, kept in [0.0, 1.0).
    /// See [`RenderUpdate::scroll_fraction`].
    scroll_fraction: f64,
    /// Parallel sniffer for DEC private mode 2031 (color-scheme notifications),
    /// which the pinned alacritty parser doesn't surface. See [`CsiModeScanner`].
    color_scheme_scanner: CsiModeScanner,
    /// Whether the running program subscribed (mode 2031) to color-scheme
    /// change notifications. When set, [`reconfigure`](Self::reconfigure) pushes
    /// an OSC 997 report so a live theme toggle re-themes the TUI without a restart.
    color_scheme_subscribed: bool,
}

impl TerminalEngine {
    pub fn new(columns: u16, rows: u16, config: EngineConfig) -> TerminalEngine {
        let size = clamped_term_size(columns, rows);
        let term_config = build_term_config(&config);
        // Length-guard: Dart always sends 18; fall back defensively if not.
        let palette: [u32; 18] = config
            .palette
            .try_into()
            .unwrap_or_else(|_| EngineConfig::default_palette());
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let replies: ReplyQueue = Arc::new(Mutex::new(Vec::new()));
        let clipboard: ClipboardReplyQueue = Arc::new(Mutex::new(Vec::new()));
        let sizes: SizeReplyQueue = Arc::new(Mutex::new(Vec::new()));
        let mut term = Term::new(
            term_config,
            &size,
            EventProxy::new(
                events.clone(),
                replies.clone(),
                clipboard.clone(),
                sizes.clone(),
            ),
        );
        // Term boots with `damage.full`; clear so the first `take_damage` after input is partial.
        term.reset_damage();
        TerminalEngine {
            term,
            parser: Processor::new(),
            events,
            replies,
            clipboard,
            sizes,
            cell_w: 0,
            cell_h: 0,
            palette,
            search: None,
            current_match: None,
            hyperlinks: Vec::new(),
            hyperlink_ids: HashMap::new(),
            scroll_fraction: 0.0,
            color_scheme_scanner: CsiModeScanner::new(),
            color_scheme_subscribed: false,
        }
    }

    pub fn take_events(&self) -> Vec<EngineEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    pub fn has_pending_clipboard(&self) -> bool {
        !self.clipboard.lock().unwrap().is_empty()
    }

    pub fn respond_clipboard_load(&mut self, text: String) {
        let pending: Vec<_> = std::mem::take(&mut *self.clipboard.lock().unwrap());
        for r in pending {
            let bytes = (r.formatter)(&text).into_bytes();
            self.events.lock().unwrap().push(EngineEvent::PtyWrite(bytes));
        }
    }

    pub fn set_cell_pixels(&mut self, w: u16, h: u16) {
        // Cell height changes the meaning of a sub-cell fraction (font zoom /
        // DPR change), so snap to a line boundary rather than carry a stale one.
        if h != self.cell_h {
            self.scroll_fraction = 0.0;
        }
        self.cell_w = w;
        self.cell_h = h;
    }

    pub fn advance(&mut self, bytes: Vec<u8>) {
        // Sniff mode-2031 toggles from the raw stream (alacritty hides them). Use
        // the original bytes — OSC pre-filtering can splice the stream, but it
        // only touches `ESC ]` sequences, never the `ESC [` CSI we look for.
        let toggles = self.color_scheme_scanner.feed(&bytes);
        let (filtered, osc_events) = extract_osc_events(&bytes);
        for e in osc_events {
            self.events.lock().unwrap().push(e);
        }
        self.parser.advance(&mut self.term, &filtered);
        self.resolve_pending_replies();
        for toggle in toggles {
            match toggle {
                ColorSchemeToggle::Subscribe => {
                    self.color_scheme_subscribed = true;
                    // Per the protocol, answer the subscribe with the current scheme.
                    self.emit_color_scheme_report();
                }
                ColorSchemeToggle::Unsubscribe => self.color_scheme_subscribed = false,
            }
        }
    }

    /// Push an OSC 997 color-scheme report (`1` = dark, `2` = light) onto the
    /// PTY write queue, derived from the live default background's luminance.
    fn emit_color_scheme_report(&self) {
        let report: &[u8] = if is_dark_bg(self.palette[17]) {
            b"\x1b]997;1\x1b\\"
        } else {
            b"\x1b]997;2\x1b\\"
        };
        self.events
            .lock()
            .unwrap()
            .push(EngineEvent::PtyWrite(report.to_vec()));
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        // Resize clamps display_offset; drop any sub-cell offset so the viewport
        // lands flush on a line after a reflow.
        self.scroll_fraction = 0.0;
        let size = clamped_term_size(columns, rows);
        self.term.resize(size);
    }

    pub fn search_is_active(&self) -> bool {
        self.search.is_some()
    }

    pub fn scroll_lines(&mut self, delta: i32) -> RenderUpdate {
        // Discrete scrolls (keyboard, wheel notches without pixel deltas) snap to
        // a line boundary — drop any accumulated sub-cell offset.
        self.scroll_fraction = 0.0;
        self.term.scroll_display(Scroll::Delta(delta));
        self.after_scroll_refresh(delta)
    }

    /// Scroll by a pixel delta with sub-cell precision. Positive `delta_px`
    /// scrolls UP into history (increasing `display_offset`); negative scrolls
    /// back down toward the live edge. The fractional remainder is kept in
    /// `scroll_fraction` ∈ [0.0, 1.0) and the line-level `display_offset` is
    /// stepped whenever the accumulator crosses a cell boundary.
    pub fn scroll_pixels(&mut self, delta_px: f64) -> RenderUpdate {
        if self.cell_h == 0 {
            // Cell height not reported yet (set_cell_pixels pending) — degrade to
            // a rounded line scroll so input is never dropped on the first frames.
            let lines = (delta_px / DEFAULT_CELL_HEIGHT_PX).round() as i32;
            if lines != 0 {
                return self.scroll_lines(lines);
            }
            return self.scroll_refresh(0);
        }
        let before = self.term.grid().display_offset();
        self.scroll_fraction += delta_px / self.cell_h as f64;
        // Step display_offset one line at a time as the accumulator crosses each
        // cell boundary. Break (and snap the fraction flush) if we hit a
        // scrollback bound so the content never floats off a hard edge.
        while self.scroll_fraction >= 1.0 {
            let step_before = self.term.grid().display_offset();
            self.term.scroll_display(Scroll::Delta(1));
            if self.term.grid().display_offset() == step_before {
                self.scroll_fraction = 0.0;
                return self.after_scroll_refresh(0);
            }
            self.scroll_fraction -= 1.0;
        }
        while self.scroll_fraction < 0.0 {
            let step_before = self.term.grid().display_offset();
            self.term.scroll_display(Scroll::Delta(-1));
            if self.term.grid().display_offset() == step_before {
                self.scroll_fraction = 0.0;
                return self.after_scroll_refresh(0);
            }
            self.scroll_fraction += 1.0;
        }
        // At the very top of history there is no line above the viewport to
        // reveal. A leftover sub-line fraction there would shift content down
        // over a blank sliver and — under a fling feeding repeated sub-line
        // deltas — oscillate (build toward 1.0, snap to 0, repeat), which reads
        // as jitter. Pin it flush.
        if self.scroll_fraction > 0.0
            && self.term.grid().display_offset() >= self.term.grid().history_size()
        {
            self.scroll_fraction = 0.0;
        }
        let after = self.term.grid().display_offset();
        let line_delta = after as i32 - before as i32;
        self.after_scroll_refresh(line_delta)
    }

    pub fn scroll_to_bottom(&mut self) -> RenderUpdate {
        let before = self.term.grid().display_offset();
        self.scroll_fraction = 0.0;
        self.term.scroll_display(Scroll::Bottom);
        let after = self.term.grid().display_offset();
        let line_delta = after as i32 - before as i32;
        self.after_scroll_refresh(line_delta)
    }

    pub fn clear_history(&mut self) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        self.term.clear_screen(ClearMode::Saved);
    }

    pub fn set_palette(&mut self, palette: Vec<u32>) {
        if let Ok(p) = palette.try_into() {
            self.palette = p;
        }
    }

    pub fn reconfigure(&mut self, config: EngineConfig) {
        self.set_palette(config.palette.clone());
        let term_config = build_term_config(&config);
        self.term.set_options(term_config);
        // A reconfigure carries the new themed palette; if the program asked for
        // color-scheme notifications (mode 2031), tell it about the new light/dark
        // so a live app-theme toggle re-themes the running TUI without a restart.
        if self.color_scheme_subscribed {
            self.emit_color_scheme_report();
        }
    }

    fn viewport_point(&self, display_row: i32, col: u16) -> Point {
        let d = self.term.grid().display_offset();
        viewport_to_point(d, Point::new(display_row.max(0) as usize, Column(col as usize)))
    }

    pub fn selection_start(&mut self, display_row: i32, col: u16, right_half: bool, kind: u8) {
        let p = self.viewport_point(display_row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        self.term.selection = Some(Selection::new(sel_type(kind), p, side));
    }

    pub fn selection_update(&mut self, display_row: i32, col: u16, right_half: bool) {
        let p = self.viewport_point(display_row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(p, side);
        }
    }

    pub fn selection_clear(&mut self) {
        self.term.selection = None;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    pub fn search_set(&mut self, pattern: String) -> bool {
        match RegexSearch::new(&pattern) {
            Ok(re) => {
                self.search = Some(re);
                self.current_match = None;
                self.search_step(Direction::Right);
                true
            }
            Err(_) => {
                self.search = None;
                self.current_match = None;
                false
            }
        }
    }

    pub fn search_next(&mut self) -> bool {
        self.search_step(Direction::Right)
    }

    pub fn search_prev(&mut self) -> bool {
        self.search_step(Direction::Left)
    }

    pub fn search_clear(&mut self) {
        self.search = None;
        self.current_match = None;
    }

    fn intern_hyperlink(&mut self, uri: &str) -> u32 {
        if let Some(&id) = self.hyperlink_ids.get(uri) {
            return id;
        }
        let id = self.hyperlinks.len() as u32 + 1;
        self.hyperlinks.push(uri.to_owned());
        self.hyperlink_ids.insert(uri.to_owned(), id);
        id
    }

    pub fn resolve_hyperlink(&self, id: u32) -> Option<String> {
        if id == 0 {
            return None;
        }
        self.hyperlinks.get((id - 1) as usize).cloned()
    }

    fn search_step(&mut self, direction: Direction) -> bool {
        if self.search.is_none() {
            return false;
        }
        let off = self.term.grid().display_offset();
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        // Origin must be ONE POINT past the current match's boundary in the
        // search direction (alacritty's `advance_search_origin`). Using the
        // boundary itself can re-find the current match → no visible change.
        let origin = match (&self.current_match, direction) {
            (Some(m), Direction::Right) => m.end().add(&self.term, Boundary::None, 1),
            (Some(m), Direction::Left) => m.start().sub(&self.term, Boundary::None, 1),
            (None, Direction::Right) => viewport_to_point(off, Point::new(0, Column(0))),
            (None, Direction::Left) => {
                viewport_to_point(off, Point::new(rows - 1, Column(cols - 1)))
            }
        };
        let re = self.search.as_mut().unwrap();
        let found = self.term.search_next(re, origin, direction, Side::Left, None);
        match found {
            Some(m) => {
                self.term.scroll_to_point(*m.start());
                self.current_match = Some(m);
                true
            }
            None => false,
        }
    }

    pub fn full_snapshot_searched(&mut self) -> RenderUpdate {
        let mut update = self.full_snapshot();
        if self.search.is_some() {
            let off = self.term.grid().display_offset();
            let rows = self.term.screen_lines();
            let cols = self.term.columns();
            let current = self.current_match.clone();
            let top = viewport_to_point(off, Point::new(0, Column(0)));
            let bottom = viewport_to_point(off, Point::new(rows - 1, Column(cols - 1)));
            let re = self.search.as_mut().unwrap();
            let matches: Vec<Match> =
                RegexIter::new(top, bottom, Direction::Right, &self.term, re).collect();
            for line in update.lines.iter_mut() {
                for col in 0..line.len() {
                    let p = viewport_to_point(off, Point::new(line.line as usize, Column(col)));
                    if matches.iter().any(|m| point_in_match(p, m)) {
                        line.flags[col] |= FLAG_MATCH;
                    }
                    if let Some(m) = &current {
                        if point_in_match(p, m) {
                            line.flags[col] |= FLAG_MATCH_CURRENT;
                        }
                    }
                }
            }
        }

        update
    }

    /// Cells of a single viewport row.
    fn line_cells(&mut self, row: usize) -> Vec<CellData> {
        let grid = self.term.grid();
        let cols = grid.columns();
        let cells_ref: Vec<Cell> = (0..cols)
            .map(|c| grid[Line(row as i32)][Column(c)].clone())
            .collect();
        cells_ref.iter().map(|c| self.cell_data(c)).collect()
    }

    fn ansi16(&self, i: u8) -> u32 {
        self.live(i as usize, self.palette[i as usize])
    }

    fn xterm256(&self, i: u8) -> u32 {
        match i {
            0..=15 => self.ansi16(i),
            16..=231 => {
                let i = i - 16;
                let r = i / 36;
                let g = (i % 36) / 6;
                let b = i % 6;
                let step = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
                pack(step(r), step(g), step(b))
            }
            232..=255 => {
                let v = 8 + (i - 232) * 10;
                pack(v, v, v)
            }
        }
    }

    fn resolve_named(&self, c: NamedColor) -> u32 {
        use NamedColor::*;
        match c {
            Foreground | BrightForeground => self.live_named(NamedColor::Foreground, self.palette[16]),
            Background => self.live_named(NamedColor::Background, self.palette[17]),
            Black => self.ansi16(0),
            Red => self.ansi16(1),
            Green => self.ansi16(2),
            Yellow => self.ansi16(3),
            Blue => self.ansi16(4),
            Magenta => self.ansi16(5),
            Cyan => self.ansi16(6),
            White => self.ansi16(7),
            BrightBlack => self.ansi16(8),
            BrightRed => self.ansi16(9),
            BrightGreen => self.ansi16(10),
            BrightYellow => self.ansi16(11),
            BrightBlue => self.ansi16(12),
            BrightMagenta => self.ansi16(13),
            BrightCyan => self.ansi16(14),
            BrightWhite => self.ansi16(15),
            Cursor => self.live_named(NamedColor::Cursor, self.palette[16]),
            DimBlack => self.ansi16(0),
            DimRed => self.ansi16(1),
            DimGreen => self.ansi16(2),
            DimYellow => self.ansi16(3),
            DimBlue => self.ansi16(4),
            DimMagenta => self.ansi16(5),
            DimCyan => self.ansi16(6),
            DimWhite => self.ansi16(7),
            DimForeground => self.live_named(NamedColor::Foreground, self.palette[16]),
        }
    }

    fn resolve_color(&self, c: Color, is_fg: bool) -> u32 {
        match c {
            Color::Named(n) => self.resolve_named(n),
            Color::Spec(Rgb { r, g, b }) => pack(r, g, b),
            Color::Indexed(i) => self.xterm256(i),
            #[allow(unreachable_patterns)]
            _ => {
                if is_fg {
                    self.palette[16]
                } else {
                    self.palette[17]
                }
            }
        }
    }

    /// Live packed color for an alacritty color-array slot, honoring runtime
    /// OSC SET. Falls back to our static config palette value when unset.
    /// Mirrors alacritty `display/content.rs`: `colors[i].unwrap_or(config[i])`.
    fn live(&self, idx: usize, fallback: u32) -> u32 {
        match self.term.colors()[idx] {
            Some(c) => pack(c.r, c.g, c.b),
            None => fallback,
        }
    }

    fn live_named(&self, n: NamedColor, fallback: u32) -> u32 {
        self.live(n as usize, fallback)
    }

    /// (default_fg, default_bg, cursor_color) for the snapshot chrome header.
    /// cursor_color is CURSOR_COLOR_UNSET unless a program set OSC 12.
    fn chrome_colors(&self) -> (u32, u32, u32) {
        let cursor_color = match self.term.colors()[NamedColor::Cursor as usize] {
            Some(c) => pack(c.r, c.g, c.b),
            None => CURSOR_COLOR_UNSET,
        };
        (
            self.live_named(NamedColor::Foreground, self.palette[16]),
            self.live_named(NamedColor::Background, self.palette[17]),
            cursor_color,
        )
    }

    /// Config-default Rgb for an OSC color index (used when term.colors[i] is
    /// unset). Mirrors alacritty's `display.colors[index]` fallback table, scoped
    /// to the indices our compact palette covers.
    fn config_default_rgb(&self, index: usize) -> Rgb {
        let packed = match index {
            0..=15 => self.palette[index],
            16..=255 => self.xterm256(index as u8),
            i if i == NamedColor::Foreground as usize => self.palette[16],
            i if i == NamedColor::Background as usize => self.palette[17],
            _ => self.palette[16],
        };
        unpack(packed)
    }

    /// Current Rgb for an OSC color query. `None` => emit no reply (matches
    /// alacritty: an unset cursor-color query is ignored).
    fn query_color_rgb(&self, index: usize) -> Option<Rgb> {
        match self.term.colors()[index] {
            Some(c) => Some(c),
            None if index == NamedColor::Cursor as usize => None,
            None => Some(self.config_default_rgb(index)),
        }
    }

    /// Drain pending OSC color queries into PtyWrite replies. Called after the
    /// parser has finished (term no longer mutably borrowed by `parser.advance`).
    fn resolve_pending_replies(&mut self) {
        let pending = std::mem::take(&mut *self.replies.lock().unwrap());
        for r in pending {
            if let Some(rgb) = self.query_color_rgb(r.index) {
                let bytes = (r.formatter)(rgb).into_bytes();
                self.events.lock().unwrap().push(EngineEvent::PtyWrite(bytes));
            }
        }

        let sizes: Vec<_> = std::mem::take(&mut *self.sizes.lock().unwrap());
        if !sizes.is_empty() {
            let size = alacritty_terminal::event::WindowSize {
                num_lines: self.term.screen_lines() as u16,
                num_cols: self.term.columns() as u16,
                cell_width: self.cell_w,
                cell_height: self.cell_h,
            };
            for r in sizes {
                let bytes = (r.formatter)(size).into_bytes();
                self.events.lock().unwrap().push(EngineEvent::PtyWrite(bytes));
            }
        }
    }

    fn cell_data(&mut self, cell: &Cell) -> CellData {
        let (hyperlink_id, hyperlink_flag) = match cell.hyperlink() {
            Some(h) => (self.intern_hyperlink(h.uri()), FLAG_HYPERLINK),
            None => (0, 0),
        };
        CellData {
            codepoint: cell.c as u32,
            fg: self.resolve_color(cell.fg, true),
            bg: self.resolve_color(cell.bg, false),
            flags: map_flags(cell.flags) | hyperlink_flag,
            hyperlink_id,
        }
    }

    fn cursor_fields(&self) -> (u32, u32, bool, u8, bool) {
        let cursor = self.term.grid().cursor.point;
        let style = self.term.cursor_style();
        let shape = match style.shape {
            CursorShape::Block => 0,
            CursorShape::Underline => 1,
            CursorShape::Beam => 2,
            CursorShape::HollowBlock => 3,
            CursorShape::Hidden => 4,
        };
        (
            cursor.line.0.max(0) as u32,
            cursor.column.0 as u32,
            self.term.mode().contains(TermMode::SHOW_CURSOR),
            shape,
            style.blinking,
        )
    }

    fn apply_wide_selection_pair(cells: &mut [CellData]) {
        for c in 0..cells.len().saturating_sub(1) {
            let lead_wide = cells[c].flags & FLAG_WIDE != 0;
            let next_spacer = cells[c + 1].flags & FLAG_WIDE_SPACER != 0;
            if lead_wide && next_spacer {
                let selected =
                    (cells[c].flags | cells[c + 1].flags) & FLAG_SELECTED != 0;
                if selected {
                    cells[c].flags |= FLAG_SELECTED;
                    cells[c + 1].flags |= FLAG_SELECTED;
                }
            }
        }
    }

    fn after_scroll_refresh(&mut self, line_delta: i32) -> RenderUpdate {
        if self.search.is_some() {
            return self.full_snapshot_searched();
        }
        let rows = self.term.screen_lines();
        if line_delta.abs() as usize >= rows {
            return self.full_snapshot();
        }
        self.scroll_refresh(line_delta)
    }

    /// Incremental scroll refresh: rotate on the Dart mirror, ship only edge
    /// rows + overscan + chrome. Falls back to [full_snapshot] when the jump is
    /// too large or search is active.
    fn scroll_refresh(&mut self, line_delta: i32) -> RenderUpdate {
        let rows = self.term.screen_lines();
        let display_offset = self.term.grid().display_offset();
        let mut lines = Vec::new();
        if line_delta > 0 {
            let d = (line_delta as usize).min(rows);
            for vp in 0..d {
                lines.push(self.pack_viewport_line(vp));
            }
        } else if line_delta < 0 {
            let d = ((-line_delta) as usize).min(rows);
            let start = rows.saturating_sub(d);
            for vp in start..rows {
                lines.push(self.pack_viewport_line(vp));
            }
        }
        lines.push(self.overscan_line_update(rows));
        let (cursor_line, cursor_col, cursor_visible, cursor_shape, cursor_blinking) =
            self.cursor_fields();
        let (default_fg, default_bg, cursor_color) = self.chrome_colors();
        RenderUpdate {
            lines,
            full: false,
            cursor_line,
            cursor_col,
            cursor_visible: cursor_visible && display_offset == 0,
            cursor_shape,
            cursor_blinking,
            mode_flags: self.term.mode().bits(),
            display_offset: display_offset as u32,
            default_fg,
            default_bg,
            cursor_color,
            scroll_fraction: self.scroll_fraction,
            scroll_line_delta: line_delta,
        }
    }

    fn pack_viewport_line(&mut self, vp_row: usize) -> LineUpdate {
        let display_offset = self.term.grid().display_offset();
        let cols = self.term.columns();
        let sel = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term));
        let collected: Vec<(usize, Cell)> = {
            let grid = self.term.grid();
            (0..cols)
                .map(|c| {
                    let p = viewport_to_point(display_offset, Point::new(vp_row, Column(c)));
                    (c, grid[p].clone())
                })
                .collect()
        };
        let mut cells = Vec::with_capacity(cols);
        for (c, cell) in collected {
            let p = viewport_to_point(display_offset, Point::new(vp_row, Column(c)));
            let mut cd = self.cell_data(&cell);
            if let Some(ref r) = sel {
                if point_in_range(p, r) {
                    cd.flags |= FLAG_SELECTED;
                }
            }
            cells.push(cd);
        }
        if sel.is_some() {
            Self::apply_wide_selection_pair(&mut cells);
        }
        LineUpdate::from_cells(vp_row as u32, cells)
    }

    fn overscan_line_update(&mut self, _rows: usize) -> LineUpdate {
        let cols = self.term.columns();
        let display_offset = self.term.grid().display_offset();
        let blank = CellData {
            codepoint: ' ' as u32,
            fg: self.live_named(NamedColor::Foreground, self.palette[16]),
            bg: self.live_named(NamedColor::Background, self.palette[17]),
            flags: 0,
            hyperlink_id: 0,
        };
        let mut cells = vec![blank; cols];
        let sel = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term));
        if (display_offset as usize) < self.term.grid().history_size() {
            let over_line = Line(-(display_offset as i32) - 1);
            let over_cells: Vec<Cell> = {
                let grid = self.term.grid();
                (0..cols).map(|c| grid[over_line][Column(c)].clone()).collect()
            };
            for (col, cell) in over_cells.into_iter().enumerate() {
                let mut cd = self.cell_data(&cell);
                if let Some(ref r) = sel {
                    if point_in_range(Point::new(over_line, Column(col)), r) {
                        cd.flags |= FLAG_SELECTED;
                    }
                }
                cells[col] = cd;
            }
            if sel.is_some() {
                Self::apply_wide_selection_pair(&mut cells);
            }
        }
        LineUpdate::from_cells(OVERSCAN_LINE_TAG, cells)
    }

    pub fn full_snapshot(&mut self) -> RenderUpdate {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let display_offset = self.term.grid().display_offset();
        let blank = CellData {
            codepoint: ' ' as u32,
            fg: self.live_named(NamedColor::Foreground, self.palette[16]),
            bg: self.live_named(NamedColor::Background, self.palette[17]),
            flags: 0,
            hyperlink_id: 0,
        };
        // One extra trailing row (`lines[rows]`) holds the overscan line that sits
        // just ABOVE the viewport top — the painter draws it in the sliver revealed
        // when `scroll_fraction > 0`. It carries the sentinel line index `rows` so
        // the hint pass skips it and the Dart side strips it off the viewport.
        let mut lines: Vec<RawLine> = (0..rows + 1)
            .map(|r| RawLine {
                line: r as u32,
                cells: vec![blank.clone(); cols],
            })
            .collect();
        let sel = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term));
        let collected: Vec<(usize, usize, Cell)> = {
            let grid = self.term.grid();
            let mut out = Vec::new();
            for indexed in grid.display_iter() {
                if let Some(vp) = point_to_viewport(display_offset, indexed.point) {
                    if vp.line < rows && vp.column.0 < cols {
                        out.push((vp.line, vp.column.0, indexed.cell.clone()));
                    }
                }
            }
            out
        };
        for (vline, vcol, cell_ref) in collected {
            let mut cd = self.cell_data(&cell_ref);
            if let Some(r) = &sel {
                let p = viewport_to_point(display_offset, Point::new(vline, Column(vcol)));
                if point_in_range(p, r) {
                    cd.flags |= FLAG_SELECTED;
                }
            }
            lines[vline].cells[vcol] = cd;
        }
        // Overscan row: the line directly above the viewport top, i.e. grid
        // `Line(-display_offset - 1)`. Present only when scrollback exists above
        // the current view (`display_offset < history_size`); otherwise the slot
        // stays blank (we're at the top of history, nothing to reveal).
        if (display_offset as usize) < self.term.grid().history_size() {
            let over_line = Line(-(display_offset as i32) - 1);
            let over_cells: Vec<Cell> = {
                let grid = self.term.grid();
                (0..cols).map(|c| grid[over_line][Column(c)].clone()).collect()
            };
            for (col, cell) in over_cells.into_iter().enumerate() {
                let mut cd = self.cell_data(&cell);
                if let Some(ref r) = sel {
                    if point_in_range(Point::new(over_line, Column(col)), r) {
                        cd.flags |= FLAG_SELECTED;
                    }
                }
                lines[rows].cells[col] = cd;
            }
        }
        // Selection is computed per-cell, but a wide (CJK) glyph spans two cells:
        // the WIDE lead cell and its trailing WIDE_SPACER. Without binding them,
        // a drag edge landing between the two would highlight only half the glyph.
        // Mirror alacritty's renderer (Selection::contains_cell): if either half of
        // a wide pair is selected, select both.
        if sel.is_some() {
            for line in lines.iter_mut() {
                for c in 0..line.cells.len().saturating_sub(1) {
                    let lead_wide = line.cells[c].flags & FLAG_WIDE != 0;
                    let next_spacer = line.cells[c + 1].flags & FLAG_WIDE_SPACER != 0;
                    if lead_wide && next_spacer {
                        let selected = (line.cells[c].flags | line.cells[c + 1].flags)
                            & FLAG_SELECTED
                            != 0;
                        if selected {
                            line.cells[c].flags |= FLAG_SELECTED;
                            line.cells[c + 1].flags |= FLAG_SELECTED;
                        }
                    }
                }
            }
        }
        let (cursor_line, cursor_col, cursor_visible, cursor_shape, cursor_blinking) =
            self.cursor_fields();
        let (default_fg, default_bg, cursor_color) = self.chrome_colors();
        RenderUpdate {
            lines: lines.into_iter().map(RawLine::pack).collect(),
            full: true,
            cursor_line,
            cursor_col,
            cursor_visible: cursor_visible && display_offset == 0,
            cursor_shape,
            cursor_blinking,
            mode_flags: self.term.mode().bits(),
            display_offset: display_offset as u32,
            default_fg,
            default_bg,
            cursor_color,
            scroll_fraction: self.scroll_fraction,
            scroll_line_delta: 0,
        }
    }

    pub fn take_damage(&mut self) -> RenderUpdate {
        // A non-zero sub-cell offset must ride on a full snapshot so the overscan
        // row and `scroll_fraction` are present. Partial damage (which ships only
        // changed viewport rows) is therefore limited to the flush, live-edge case.
        if self.term.grid().display_offset() > 0 || self.scroll_fraction != 0.0 {
            return self.full_snapshot();
        }
        // Collect damaged viewport rows, then drop the borrow before reading cells.
        let damaged: Option<Vec<usize>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(it) => Some(it.map(|b| b.line).collect()),
        };
        self.term.reset_damage();

        let (cursor_line, cursor_col, cursor_visible, cursor_shape, cursor_blinking) =
            self.cursor_fields();
        let u = match damaged {
            None => {
                let mut u = self.full_snapshot();
                u.cursor_line = cursor_line;
                u.cursor_col = cursor_col;
                u.cursor_visible = cursor_visible;
                u.cursor_shape = cursor_shape;
                u.cursor_blinking = cursor_blinking;
                u
            }
            Some(mut rows) => {
                rows.sort_unstable();
                rows.dedup();
                let lines = rows
                    .into_iter()
                    .map(|row| LineUpdate::from_cells(row as u32, self.line_cells(row)))
                    .collect();
                let (default_fg, default_bg, cursor_color) = self.chrome_colors();
                RenderUpdate {
                    lines,
                    full: false,
                    cursor_line,
                    cursor_col,
                    cursor_visible,
                    cursor_shape,
                    cursor_blinking,
                    mode_flags: self.term.mode().bits(),
                    display_offset: 0,
                    default_fg,
                    default_bg,
                    cursor_color,
                    scroll_fraction: 0.0,
                    scroll_line_delta: 0,
                }
            }
        };
        u
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(cols: u16, rows: u16) -> TerminalEngine {
        TerminalEngine::new(cols, rows, EngineConfig::defaults())
    }

    fn line<'a>(u: &'a RenderUpdate, row: u32) -> &'a LineUpdate {
        u.lines.iter().find(|l| l.line == row).expect("line present")
    }
    fn ch(u: &RenderUpdate, row: u32, col: usize) -> char {
        char::from_u32(line(u, row).cell(col).codepoint).unwrap()
    }

    fn pty_writes(e: &TerminalEngine) -> Vec<Vec<u8>> {
        e.take_events()
            .into_iter()
            .filter_map(|ev| match ev {
                EngineEvent::PtyWrite(b) => Some(b),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn osc11_set_changes_default_bg_and_cells() {
        let mut e = engine(10, 3);
        // OSC 11 ; rgb:ff/00/00  (set default background to pure red), ST-terminated.
        e.advance(b"\x1b]11;rgb:ff/00/00\x1b\\".to_vec());
        let u = e.full_snapshot();
        assert_eq!(u.default_bg, 0x00FF_0000, "chrome default_bg follows OSC 11");
        // A blank cell (no explicit bg) must repack to the live default bg.
        assert_eq!(line(&u, 0).cell(0).bg, 0x00FF_0000, "blank cell uses live bg");
    }

    #[test]
    fn degenerate_size_clamps_to_vt_minimum_and_never_panics() {
        // Regression: a 1-column grid panics `cursor_cell` when `Term::input`
        // writes a fullwidth glyph's trailing spacer past the row end. Both the
        // constructor and resize must clamp to the VT minimum at the boundary.
        let mut e = engine(1, 1);
        assert!(e.term.columns() >= MIN_COLUMNS, "new() clamps columns");
        assert!(e.term.screen_lines() >= MIN_SCREEN_LINES, "new() clamps rows");
        // Fullwidth (CJK) + ASCII into the formerly-degenerate grid: no panic.
        e.advance("界A".as_bytes().to_vec());

        // A live resize down to one column must clamp too, then tolerate output.
        e.resize(1, 1);
        assert!(e.term.columns() >= MIN_COLUMNS, "resize() clamps columns");
        e.advance("界".as_bytes().to_vec());
    }

    #[test]
    fn osc111_reset_reverts_default_bg_to_config() {
        let mut e = engine(10, 3);
        e.advance(b"\x1b]11;rgb:ff/00/00\x1b\\".to_vec());
        e.advance(b"\x1b]111\x1b\\".to_vec()); // reset default background
        let u = e.full_snapshot();
        assert_eq!(u.default_bg, EngineConfig::default_palette()[17]);
    }

    #[test]
    fn cursor_color_unset_sentinel_then_osc12() {
        let mut e = engine(10, 3);
        assert_eq!(e.full_snapshot().cursor_color, CURSOR_COLOR_UNSET);
        e.advance(b"\x1b]12;rgb:00/ff/00\x1b\\".to_vec()); // set cursor color green
        assert_eq!(e.full_snapshot().cursor_color, 0x0000_FF00);
    }

    #[test]
    fn osc11_query_emits_background_reply() {
        let mut e = engine(10, 3);
        e.advance(b"\x1b]11;?\x1b\\".to_vec()); // query default background
        let writes = pty_writes(&e);
        assert_eq!(writes.len(), 1, "exactly one reply");
        assert!(
            writes[0].starts_with(b"\x1b]11;rgb:"),
            "OSC 11 reply prefix, got {:?}",
            String::from_utf8_lossy(&writes[0])
        );
    }

    #[test]
    fn osc11_query_reflects_live_color() {
        let mut e = engine(10, 3);
        e.advance(b"\x1b]11;rgb:ff/00/00\x1b\\".to_vec()); // set red
        let _ = e.take_events(); // drain set (no reply for SET)
        e.advance(b"\x1b]11;?\x1b\\".to_vec()); // query
        let writes = pty_writes(&e);
        let s = String::from_utf8_lossy(&writes[0]);
        assert!(s.contains("rgb:ffff/0000/0000"), "reply carries SET value, got {s}");
    }

    #[test]
    fn osc12_query_ignored_when_cursor_unset() {
        let mut e = engine(10, 3);
        e.advance(b"\x1b]12;?\x1b\\".to_vec()); // query cursor color, never set
        assert!(pty_writes(&e).is_empty(), "unset cursor query is dropped (alacritty parity)");
    }

    /// EngineConfig whose default background is `bg` (packed RGB); rest defaults.
    fn config_with_bg(bg: u32) -> EngineConfig {
        let mut palette = EngineConfig::default_palette().to_vec();
        palette[17] = bg;
        EngineConfig {
            palette,
            ..EngineConfig::defaults()
        }
    }

    const COLOR_SCHEME_DARK: &[u8] = b"\x1b]997;1\x1b\\";
    const COLOR_SCHEME_LIGHT: &[u8] = b"\x1b]997;2\x1b\\";

    #[test]
    fn mode2031_subscribe_reports_current_scheme() {
        // Default config bg (0x181818) is dark → report "1".
        let mut e = engine(10, 3);
        e.advance(b"\x1b[?2031h".to_vec());
        assert_eq!(pty_writes(&e), vec![COLOR_SCHEME_DARK.to_vec()]);
    }

    #[test]
    fn mode2031_reconfigure_pushes_update_when_subscribed() {
        let mut e = engine(10, 3);
        e.advance(b"\x1b[?2031h".to_vec());
        let _ = e.take_events(); // drain the subscribe report
        // Switch to a light themed palette: the running TUI must be told.
        e.reconfigure(config_with_bg(0x00FF_FFFF));
        assert_eq!(pty_writes(&e), vec![COLOR_SCHEME_LIGHT.to_vec()]);
    }

    #[test]
    fn mode2031_reconfigure_silent_when_not_subscribed() {
        let mut e = engine(10, 3);
        e.reconfigure(config_with_bg(0x00FF_FFFF));
        assert!(pty_writes(&e).is_empty(), "no report without a subscriber");
    }

    #[test]
    fn mode2031_unsubscribe_stops_updates() {
        let mut e = engine(10, 3);
        e.advance(b"\x1b[?2031h".to_vec());
        e.advance(b"\x1b[?2031l".to_vec());
        let _ = e.take_events();
        e.reconfigure(config_with_bg(0x00FF_FFFF));
        assert!(pty_writes(&e).is_empty(), "unsubscribed program gets no updates");
    }

    #[test]
    fn writes_plain_text_into_the_grid() {
        let mut e = engine(20, 5);
        e.advance(b"hi".to_vec());
        let u = e.full_snapshot();
        assert_eq!(u.columns(), 20);
        // screen_lines (5) viewport rows + 1 trailing overscan row.
        assert_eq!(u.lines.len(), 6);
        assert_eq!(ch(&u, 0, 0), 'h');
        assert_eq!(ch(&u, 0, 1), 'i');
        assert!(u.full);
    }

    #[test]
    fn applies_sgr_foreground_color() {
        let mut e = engine(20, 5);
        e.advance(b"\x1b[31mR".to_vec());
        let u = e.full_snapshot();
        let c = &line(&u, 0).cell(0);
        assert_eq!(char::from_u32(c.codepoint).unwrap(), 'R');
        assert_eq!(c.fg & 0x00FF_FFFF, 0x00CC_0000);
    }

    #[test]
    fn newline_moves_to_next_row() {
        let mut e = engine(20, 5);
        e.advance(b"a\r\nb".to_vec());
        let u = e.full_snapshot();
        assert_eq!(ch(&u, 0, 0), 'a');
        assert_eq!(ch(&u, 1, 0), 'b');
    }

    #[test]
    fn resize_changes_reported_dimensions() {
        let mut e = engine(20, 5);
        e.resize(40, 10);
        let u = e.full_snapshot();
        assert_eq!(u.columns(), 40);
        assert_eq!(u.lines.len(), 11); // 10 viewport + 1 overscan
        assert_eq!(line(&u, 0).len(), 40);
    }

    #[test]
    fn damage_reports_only_changed_lines_then_resets() {
        let mut e = engine(20, 5);
        e.advance(b"hi".to_vec());
        let u = e.take_damage();
        assert!(!u.full);
        assert!(u.lines.iter().any(|l| l.line == 0));
        assert_eq!(
            char::from_u32(
                u.lines
                    .iter()
                    .find(|l| l.line == 0)
                    .unwrap()
                    .cell(0)
                    .codepoint
            )
            .unwrap(),
            'h'
        );
        // Second read after reset: no new cell writes; alacritty_terminal may still
        // report cursor-cell damage from `damage()` (at most one line).
        let u2 = e.take_damage();
        assert!(!u2.full);
        assert!(u2.lines.len() <= 1);
    }

    #[test]
    fn resize_forces_full_damage() {
        let mut e = engine(20, 5);
        e.take_damage(); // drain initial
        e.resize(30, 6);
        let u = e.take_damage();
        assert!(u.full);
        assert_eq!(u.lines.len(), 7); // 6 viewport + 1 overscan
    }

    #[test]
    fn wide_char_sets_wide_and_spacer_flags() {
        let mut e = engine(20, 5);
        e.advance("中".as_bytes().to_vec());
        let u = e.full_snapshot();
        let row0 = line(&u, 0);
        assert_eq!(char::from_u32(row0.cell(0).codepoint).unwrap(), '中');
        assert_ne!(row0.cell(0).flags & FLAG_WIDE, 0, "lead cell must be WIDE_CHAR");
        assert_ne!(
            row0.cell(1).flags & FLAG_WIDE_SPACER,
            0,
            "the cell after a wide char must be WIDE_CHAR_SPACER"
        );
        assert_eq!(
            char::from_u32(row0.cell(1).codepoint).unwrap(),
            ' ',
            "WIDE_CHAR_SPACER cell.c is a space placeholder"
        );
    }

    #[test]
    fn cursor_style_shape_and_blinking_exposed() {
        let mut e = engine(20, 5);
        e.advance(b"\x1b[5 q".to_vec()); // DECSCUSR 5 = blinking bar (beam)
        let u = e.full_snapshot();
        assert_eq!(u.cursor_shape, 2); // beam
        assert!(u.cursor_blinking);

        let mut e2 = engine(20, 5);
        e2.advance(b"\x1b[2 q".to_vec()); // 2 = steady block
        let u2 = e2.full_snapshot();
        assert_eq!(u2.cursor_shape, 0); // block
        assert!(!u2.cursor_blinking);
    }

    #[test]
    fn maps_dim_and_strikeout_flags() {
        let mut e = engine(20, 5);
        e.advance(b"\x1b[2mD".to_vec()); // SGR 2 = dim
        assert_ne!(line(&e.full_snapshot(), 0).cell(0).flags & FLAG_DIM, 0);

        let mut e2 = engine(20, 5);
        e2.advance(b"\x1b[9mS".to_vec()); // SGR 9 = strikeout
        assert_ne!(
            line(&e2.full_snapshot(), 0).cell(0).flags & FLAG_STRIKEOUT,
            0
        );
    }

    #[test]
    fn mode_flags_reflect_private_modes() {
        let mut e = engine(20, 5);
        e.advance(b"\x1b[?1h".to_vec()); // DECCKM -> APP_CURSOR (1<<1)
        assert_ne!(e.full_snapshot().mode_flags & (1 << 1), 0);
        e.advance(b"\x1b[?2004h".to_vec()); // bracketed paste (1<<4)
        assert_ne!(e.full_snapshot().mode_flags & (1 << 4), 0);
        e.advance(b"\x1b[?1l".to_vec()); // reset DECCKM
        assert_eq!(e.full_snapshot().mode_flags & (1 << 1), 0);
    }

    #[test]
    fn scroll_shows_history_then_returns_to_bottom() {
        let mut e = engine(10, 3); // 3 visible rows
        for i in 0..10 {
            e.advance(format!("line{}\r\n", i).into_bytes());
        }
        // At bottom: latest lines visible, offset 0.
        assert_eq!(e.full_snapshot().display_offset, 0);

        e.scroll_lines(2); // scroll up 2 lines into history
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 2);
        // Row 0 now shows an older line than it did at the bottom.
        let row0: String = u.lines.iter().find(|l| l.line == 0).unwrap()
            .codepoints.iter().map(|c| char::from_u32(*c).unwrap()).collect();
        assert!(row0.trim_end().starts_with("line"));
        // Cursor is hidden while scrolled back.
        assert!(!u.cursor_visible);

        e.scroll_to_bottom();
        assert_eq!(e.full_snapshot().display_offset, 0);
    }

    #[test]
    fn scroll_refresh_ships_edge_rows_not_full_grid() {
        let mut e = engine(10, 5);
        for i in 0..20 {
            e.advance(format!("{:02}\r\n", i).into_bytes());
        }
        let u = e.scroll_lines(1);
        assert!(!u.full);
        assert_eq!(u.scroll_line_delta, 1);
        assert_eq!(u.lines.len(), 2, "viewport edge + overscan");
        assert!(u.lines.iter().any(|l| l.line == 0));
        assert!(u.lines.iter().any(|l| l.line == OVERSCAN_LINE_TAG));
        let scrolled_full = e.full_snapshot();
        let edge = u.lines.iter().find(|l| l.line == 0).unwrap();
        let full_r0 = scrolled_full.lines.iter().find(|l| l.line == 0).unwrap();
        assert_eq!(edge.codepoints, full_r0.codepoints);
    }

    #[test]
    fn selection_text_and_selected_flag() {
        let mut e = engine(20, 3);
        e.advance(b"hello world".to_vec());
        e.selection_start(0, 0, false, 0); // simple, row 0 col 0, left side
        e.selection_update(0, 4, true); // through col 4, right side -> "hello"
        let txt = e.selection_text().unwrap();
        assert!(txt.starts_with("hello"), "got {:?}", txt);

        let u = e.full_snapshot();
        let row0 = u.lines.iter().find(|l| l.line == 0).unwrap();
        assert_ne!(row0.cell(0).flags & FLAG_SELECTED, 0);
        assert_ne!(row0.cell(4).flags & FLAG_SELECTED, 0);
        assert_eq!(row0.cell(10).flags & FLAG_SELECTED, 0); // 'd' not selected

        e.selection_clear();
        assert!(e.selection_text().is_none());
    }

    #[test]
    fn wide_char_selection_never_splits_glyph() {
        // "中文" occupies cols 0-1 (中: WIDE lead + WIDE_SPACER) and 2-3 (文: ditto).
        // A drag edge that lands between a glyph's two cells must still select the
        // whole glyph, never half. Each case below puts a selection boundary inside
        // exactly one glyph; without the wide-pair binding one of the two cells of
        // that glyph would be left unselected.
        let cases = [
            // end at col 2 (lead of 文, right side) -> 文's spacer (col 3) excluded.
            (0u16, false, 2u16, true),
            // start at col 1 (spacer of 中, left side) -> 中's lead (col 0) excluded.
            (1, false, 3, true),
        ];
        for (start_col, start_right, end_col, end_right) in cases {
            let mut e = engine(20, 3);
            e.advance("中文".as_bytes().to_vec());
            e.selection_start(0, start_col, start_right, 0);
            e.selection_update(0, end_col, end_right);
            let u = e.full_snapshot();
            let row0 = u.lines.iter().find(|l| l.line == 0).unwrap();
            let sel = |c: usize| row0.cell(c).flags & FLAG_SELECTED != 0;
            // At least one cell must be selected (guards against an empty range that
            // would make the equality checks below pass vacuously).
            assert!(
                sel(0) || sel(1) || sel(2) || sel(3),
                "selection produced no selected cells for \
                 start=({start_col},{start_right}) end=({end_col},{end_right})"
            );
            assert_eq!(sel(0), sel(1), "中 half-selected");
            assert_eq!(sel(2), sel(3), "文 half-selected");
        }
    }

    #[test]
    fn dsr_emits_pty_write() {
        use crate::event_proxy::EngineEvent;
        let mut e = engine(20, 5);
        e.advance(b"\x1b[6n".to_vec()); // DSR: report cursor position (1-based row;col R)
        let events = e.take_events();
        let report = events
            .iter()
            .find_map(|ev| match ev {
                EngineEvent::PtyWrite(b) => Some(b.as_slice()),
                _ => None,
            })
            .expect("expected a PtyWrite cursor report");
        assert_eq!(report, b"\x1b[1;1R");
    }

    #[test]
    fn reconfigure_updates_scrollback_and_palette() {
        let mut e = TerminalEngine::new(10, 2, EngineConfig::defaults());
        let mut cfg = EngineConfig::defaults();
        cfg.scrollback = 5;
        cfg.palette[1] = 0x00AB_CDEF;
        e.reconfigure(cfg);
        e.advance(b"\x1b[31mR".to_vec());
        let snap = e.full_snapshot();
        let red_cell = &snap.lines[0].cell(0);
        assert_eq!(red_cell.fg & 0x00FF_FFFF, 0x00AB_CDEF);
    }

    #[test]
    fn semantic_escape_chars_affect_word_selection() {
        let mut cfg = EngineConfig::defaults();
        cfg.semantic_escape_chars = "-".to_string();
        let mut e = TerminalEngine::new(20, 2, cfg);
        e.advance(b"foo-bar".to_vec());
        e.selection_start(0, 0, false, 1);
        e.selection_update(0, 0, false);
        assert_eq!(e.selection_text().as_deref(), Some("foo"));
    }

    #[test]
    fn palette_injection_overrides_ansi_colors() {
        let mut pal = EngineConfig::default_palette();
        pal[1] = 0x0011_2233;
        let cfg = EngineConfig {
            palette: pal.to_vec(),
            scrollback: 1000,
            ..EngineConfig::defaults()
        };
        let mut e = TerminalEngine::new(20, 5, cfg);
        e.advance(b"\x1b[31mR".to_vec());
        let u = e.full_snapshot();
        assert_eq!(u.lines[0].cell(0).fg & 0x00FF_FFFF, 0x0011_2233);
    }

    #[test]
    fn default_palette_matches_v1_table() {
        let p = EngineConfig::default_palette();
        assert_eq!(p[0], 0x0000_0000);
        assert_eq!(p[1], 0x00CC_0000);
        assert_eq!(p[15], 0x00EE_EEEC);
        assert_eq!(p[16], 0x00D8_D8D8);
        assert_eq!(p[17], 0x0018_1818);
    }

    #[test]
    fn custom_scrollback_is_honored() {
        let mut cfg = EngineConfig::defaults();
        cfg.scrollback = 50;
        let mut e = TerminalEngine::new(10, 3, cfg);
        for _ in 0..200 { e.advance(b"x\r\n".to_vec()); }
        e.scroll_lines(1000);
        let u = e.full_snapshot();
        assert!(u.display_offset <= 50, "offset {} exceeds history 50", u.display_offset);
    }

    #[test]
    fn search_set_marks_matches_and_focuses_first() {
        let mut e = engine(20, 5);
        e.advance(b"foo bar foo".to_vec());
        assert!(e.search_set("foo".to_string()));
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cell(0).flags & FLAG_MATCH, 0);
        assert_ne!(u.lines[0].cell(0).flags & FLAG_MATCH_CURRENT, 0);
        assert_ne!(u.lines[0].cell(8).flags & FLAG_MATCH, 0);
        assert_eq!(u.lines[0].cell(8).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cell(3).flags & (FLAG_MATCH | FLAG_MATCH_CURRENT), 0);
    }

    #[test]
    fn search_next_moves_focus_to_the_second_match() {
        let mut e = engine(20, 5);
        e.advance(b"foo bar foo".to_vec());
        e.search_set("foo".to_string());
        assert!(e.search_next());
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cell(8).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cell(0).flags & FLAG_MATCH_CURRENT, 0);
    }

    #[test]
    fn search_prev_walks_back_then_re_finds_the_first_match() {
        // Regression: origin was sitting AT m.start() for Direction::Left, so
        // search_next could re-find the current match → no visible change.
        // Fix uses Point::sub(.., Boundary::None, 1) to step past it.
        let mut e = engine(20, 5);
        e.advance(b"foo bar foo".to_vec());
        e.search_set("foo".to_string());      // first "foo" (col 0) focused
        assert!(e.search_next());              // second "foo" (col 8) focused
        let u1 = e.full_snapshot_searched();
        assert_ne!(u1.lines[0].cell(8).flags & FLAG_MATCH_CURRENT, 0);
        assert!(e.search_prev());              // back to first "foo"
        let u2 = e.full_snapshot_searched();
        assert_ne!(u2.lines[0].cell(0).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u2.lines[0].cell(8).flags & FLAG_MATCH_CURRENT, 0);
    }

    #[test]
    fn search_next_and_prev_cycle_multiple_matches_on_one_line() {
        // Three "foo" on a single line at cols 0-2, 3-5, 6-8 — the user's
        // failing scenario ("同行多匹配 ↓ 不动").
        let mut e = engine(20, 5);
        e.advance(b"foofoofoo".to_vec());
        assert!(e.search_set("foo".to_string()));
        // After set: focus on first.
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cell(0).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cell(3).flags & FLAG_MATCH_CURRENT, 0);

        // next → second.
        assert!(e.search_next());
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cell(0).flags & FLAG_MATCH_CURRENT, 0);
        assert_ne!(u.lines[0].cell(3).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cell(6).flags & FLAG_MATCH_CURRENT, 0);

        // next → third.
        assert!(e.search_next());
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cell(3).flags & FLAG_MATCH_CURRENT, 0);
        assert_ne!(u.lines[0].cell(6).flags & FLAG_MATCH_CURRENT, 0);

        // prev → second.
        assert!(e.search_prev());
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cell(3).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cell(6).flags & FLAG_MATCH_CURRENT, 0);

        // prev → first.
        assert!(e.search_prev());
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cell(0).flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cell(3).flags & FLAG_MATCH_CURRENT, 0);
    }

    #[test]
    fn invalid_regex_returns_false_and_highlights_nothing() {
        let mut e = engine(20, 5);
        e.advance(b"foo".to_vec());
        assert!(!e.search_set("(".to_string()));
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cell(0).flags & FLAG_MATCH, 0);
    }

    #[test]
    fn search_clear_removes_highlight() {
        let mut e = engine(20, 5);
        e.advance(b"foo".to_vec());
        e.search_set("foo".to_string());
        e.search_clear();
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cell(0).flags & FLAG_MATCH, 0);
    }

    #[test]
    fn osc8_hyperlink_is_carried_on_cell_data() {
        let mut e = engine(20, 3);
        e.advance(b"\x1b]8;;https://example.com\x1b\\X\x1b]8;;\x1b\\".to_vec());
        let u = e.full_snapshot_searched();
        let cell = &u.lines[0].cell(0);
        assert_ne!(cell.flags & FLAG_HYPERLINK, 0);
        assert_ne!(cell.hyperlink_id, 0);
        assert_eq!(
            e.resolve_hyperlink(cell.hyperlink_id).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn resolve_hyperlink_returns_none_for_unknown_id() {
        let e = engine(10, 3);
        assert!(e.resolve_hyperlink(0).is_none());
        assert!(e.resolve_hyperlink(999).is_none());
    }

    #[test]
    fn clear_history_drops_scrollback() {
        let mut e = TerminalEngine::new(10, 2, EngineConfig::defaults());
        // produce > screen_lines of output so there is scrollback
        e.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n".to_vec());
        e.scroll_lines(100); // scroll up into history
        let before = e.full_snapshot().display_offset;
        e.clear_history();
        let after = e.full_snapshot().display_offset;
        assert_eq!(after, 0);
        assert!(before >= after);
    }

    #[test]
    fn plain_url_is_not_auto_marked_by_engine() {
        // URL detection moved to the Dart UrlLinkProvider; the engine marks only
        // OSC 8 hyperlinks. A bare URL with no OSC 8 must stay unmarked here.
        let mut e = engine(40, 3);
        e.advance(b"see https://example.com here".to_vec());
        let u = e.full_snapshot_searched();
        let any_hyperlink = u
            .lines
            .iter()
            .flat_map(|l| l.flags.iter())
            .any(|f| f & FLAG_HYPERLINK != 0);
        assert!(
            !any_hyperlink,
            "engine must not auto-mark plain URLs after hint-pass removal"
        );
    }

    #[test]
    fn osc52_paste_round_trips_when_enabled() {
        let mut cfg = EngineConfig::defaults();
        cfg.osc52 = 3;
        let mut e = TerminalEngine::new(10, 2, cfg);
        e.advance(b"\x1b]52;c;?\x07".to_vec());
        assert!(e.has_pending_clipboard());
        e.respond_clipboard_load("hello".to_string());
        let evs = e.take_events();
        let wrote = evs.iter().any(|ev| {
            matches!(ev, EngineEvent::PtyWrite(b)
                if String::from_utf8_lossy(b).contains("52;c;")
                && String::from_utf8_lossy(b).contains("aGVsbG8="))
        });
        assert!(wrote, "expected base64('hello')=aGVsbG8= reply, got {evs:?}");
    }

    #[test]
    fn osc52_paste_denied_by_default() {
        let mut e = TerminalEngine::new(10, 2, EngineConfig::defaults());
        e.advance(b"\x1b]52;c;?\x07".to_vec());
        assert!(!e.has_pending_clipboard());
    }

    #[test]
    fn text_area_size_request_replies_with_pixels() {
        let mut e = TerminalEngine::new(80, 24, EngineConfig::defaults());
        e.set_cell_pixels(9, 18);
        e.advance(b"\x1b[14t".to_vec());
        let evs = e.take_events();
        let ok = evs.iter().any(|ev| {
            matches!(ev, EngineEvent::PtyWrite(b)
                if String::from_utf8_lossy(b).contains("4;432;720t"))
        });
        assert!(ok, "got {evs:?}");
    }

    // ---- sub-cell pixel scroll ------------------------------------------

    /// Print `n` numbered lines so a small screen accrues scrollback history.
    fn fill_lines(e: &mut TerminalEngine, n: usize) {
        for i in 0..n {
            e.advance(format!("line{i:02}\r\n").into_bytes());
        }
    }

    #[test]
    fn scroll_pixels_accumulates_fraction_then_steps_a_line() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 12);
        e.set_cell_pixels(9, 18);

        // Half a cell up: stays on the same line, fraction = 0.5.
        e.scroll_pixels(9.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 0);
        assert!((u.scroll_fraction - 0.5).abs() < 1e-9);

        // Another half: wraps to the next history line, fraction back to 0.
        e.scroll_pixels(9.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 1);
        assert!(u.scroll_fraction.abs() < 1e-9);

        // Reverse half a cell: steps back down, fraction = 0.5 again.
        e.scroll_pixels(-9.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 0);
        assert!((u.scroll_fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn scroll_pixels_large_delta_steps_multiple_lines() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 20);
        e.set_cell_pixels(9, 18);
        e.scroll_pixels(3.5 * 18.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 3);
        assert!((u.scroll_fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn scroll_pixels_zero_delta_is_noop() {
        let mut e = engine(10, 3);
        e.set_cell_pixels(9, 18);
        e.scroll_pixels(0.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 0);
        assert_eq!(u.scroll_fraction, 0.0);
    }

    #[test]
    fn scroll_pixels_without_cell_pixels_falls_back_to_line_scroll() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 12);
        e.scroll_pixels(2.0 * DEFAULT_CELL_HEIGHT_PX);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 2);
        assert_eq!(u.scroll_fraction, 0.0);
    }

    #[test]
    fn scroll_pixels_snaps_flush_at_top_of_history() {
        let mut e = engine(10, 2);
        fill_lines(&mut e, 6);
        e.set_cell_pixels(9, 18);
        // Scroll far past the top; offset clamps and the fraction snaps to 0.
        e.scroll_pixels(1000.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset as usize, e.term.grid().history_size());
        assert_eq!(u.scroll_fraction, 0.0);
    }

    #[test]
    fn scroll_pixels_sub_line_at_top_keeps_fraction_pinned() {
        // Regression: at the very top, sub-line up-scrolls (e.g. a decaying fling)
        // must not accumulate a fraction — that caused build-to-1/snap jitter.
        let mut e = engine(10, 2);
        fill_lines(&mut e, 5);
        e.set_cell_pixels(9, 18);
        e.scroll_pixels(1000.0); // pin to the top
        assert_eq!(e.full_snapshot().display_offset as usize, e.term.grid().history_size());
        for _ in 0..5 {
            e.scroll_pixels(9.0); // half a cell up, repeatedly
            assert_eq!(e.full_snapshot().scroll_fraction, 0.0);
        }
    }

    #[test]
    fn scroll_pixels_snaps_flush_at_live_bottom() {
        let mut e = engine(10, 2);
        fill_lines(&mut e, 6);
        e.set_cell_pixels(9, 18);
        // Already at the bottom; scrolling further down cannot move and snaps flush.
        e.scroll_pixels(-1000.0);
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 0);
        assert_eq!(u.scroll_fraction, 0.0);
    }

    #[test]
    fn full_snapshot_overscan_row_is_line_above_viewport() {
        // 2-row screen, four logical lines: viewport shows CCCC/DDDD at offset 0,
        // so the overscan row (last entry) must be the line just above: BBBB.
        let mut e = engine(10, 2);
        e.advance(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD".to_vec());
        let u = e.full_snapshot();
        assert_eq!(u.lines.len(), 3); // 2 viewport + 1 overscan
        assert_eq!(ch(&u, 0, 0), 'C');
        assert_eq!(ch(&u, 1, 0), 'D');
        // Overscan carries sentinel line index == screen_lines.
        assert_eq!(ch(&u, 2, 0), 'B');
    }

    #[test]
    fn full_snapshot_overscan_blank_at_top_of_history() {
        // No scrollback above the viewport → overscan row left blank.
        let mut e = engine(10, 3);
        e.advance(b"hi".to_vec());
        let u = e.full_snapshot();
        assert_eq!(u.lines.len(), 4);
        assert_eq!(ch(&u, 3, 0), ' ');
    }

    #[test]
    fn take_damage_forces_full_when_fraction_nonzero() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 12);
        e.set_cell_pixels(9, 18);
        e.take_damage(); // drain
        e.scroll_pixels(9.0); // fraction 0.5, still display_offset 0
        let u = e.take_damage();
        assert!(u.full, "non-zero fraction must ride a full snapshot");
        assert!((u.scroll_fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn resize_clears_scroll_fraction() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 12);
        e.set_cell_pixels(9, 18);
        e.scroll_pixels(9.0); // fraction 0.5
        e.resize(12, 4);
        assert_eq!(e.full_snapshot().scroll_fraction, 0.0);
    }

    #[test]
    fn cell_height_change_clears_scroll_fraction() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 12);
        e.set_cell_pixels(9, 18);
        e.scroll_pixels(9.0); // fraction 0.5
        e.set_cell_pixels(9, 20); // zoom: cell height changed
        assert_eq!(e.full_snapshot().scroll_fraction, 0.0);
        // Same height again must NOT reset a live fraction.
        e.scroll_pixels(10.0); // 0.5 at h=20
        e.set_cell_pixels(9, 20);
        assert!((e.full_snapshot().scroll_fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn scroll_to_bottom_clears_fraction() {
        let mut e = engine(10, 3);
        fill_lines(&mut e, 12);
        e.set_cell_pixels(9, 18);
        e.scroll_pixels(9.0);
        e.scroll_to_bottom();
        let u = e.full_snapshot();
        assert_eq!(u.display_offset, 0);
        assert_eq!(u.scroll_fraction, 0.0);
    }
}
