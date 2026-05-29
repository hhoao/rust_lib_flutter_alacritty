use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::event_proxy::{
    ClipboardReplyQueue, EngineEvent, EventProxy, EventQueue, ReplyQueue, SizeReplyQueue,
};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{point_to_viewport, viewport_to_point, Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};

/// Flat, FFI-friendly cell. fg/bg are packed 0x00RRGGBB.
#[derive(Clone, Debug)]
pub struct CellData {
    pub codepoint: u32,
    pub fg: u32,
    pub bg: u32,
    pub flags: u16,
    pub hyperlink_id: u32,
}

#[derive(Clone, Debug)]
pub struct LineUpdate {
    pub line: u32,
    pub cells: Vec<CellData>,
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
}

impl RenderUpdate {
    /// Column count inferred from the first line (test/diagnostic helper).
    pub fn columns(&self) -> usize {
        self.lines.first().map(|l| l.cells.len()).unwrap_or(0)
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
pub const FLAG_MATCH: u16 = 1 << 9;
pub const FLAG_MATCH_CURRENT: u16 = 1 << 10;
pub const FLAG_HYPERLINK: u16 = 1 << 11;

const DEFAULT_FG: u32 = 0x00D8_D8D8;
const DEFAULT_BG: u32 = 0x0018_1818;

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
    hint_regex: Option<RegexSearch>,
}

impl TerminalEngine {
    pub fn new(columns: u16, rows: u16, config: EngineConfig) -> TerminalEngine {
        let size = alacritty_terminal::term::test::TermSize::new(
            columns as usize,
            rows as usize,
        );
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
        let hint_regex = RegexSearch::new(r"(?:https?|ftp|file)://[^\s]+").ok();
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
            hint_regex,
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
        self.cell_w = w;
        self.cell_h = h;
    }

    pub fn advance(&mut self, bytes: Vec<u8>) {
        self.parser.advance(&mut self.term, &bytes);
        self.resolve_pending_replies();
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        let size = alacritty_terminal::term::test::TermSize::new(
            columns as usize,
            rows as usize,
        );
        self.term.resize(size);
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn clear_history(&mut self) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        self.term.clear_screen(ClearMode::Saved);
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
                for col in 0..line.cells.len() {
                    let p = viewport_to_point(off, Point::new(line.line as usize, Column(col)));
                    if matches.iter().any(|m| point_in_match(p, m)) {
                        line.cells[col].flags |= FLAG_MATCH;
                    }
                    if let Some(m) = &current {
                        if point_in_match(p, m) {
                            line.cells[col].flags |= FLAG_MATCH_CURRENT;
                        }
                    }
                }
            }
        }

        self.apply_hint_pass(&mut update);
        update
    }

    /// URL auto-detect over the visible region; skips cells already hyperlinked (OSC 8).
    fn apply_hint_pass(&mut self, update: &mut RenderUpdate) {
        if let Some(hint) = self.hint_regex.as_mut() {
            let off = self.term.grid().display_offset();
            let rows = self.term.screen_lines();
            let cols = self.term.columns();
            let top = viewport_to_point(off, Point::new(0, Column(0)));
            let bottom = viewport_to_point(off, Point::new(rows - 1, Column(cols - 1)));
            let matches: Vec<Match> =
                RegexIter::new(top, bottom, Direction::Right, &self.term, hint).collect();
            let uris_for_matches: Vec<String> = matches
                .iter()
                .map(|m| {
                    let mut s = String::new();
                    let grid = self.term.grid();
                    let (start, end) = (m.start(), m.end());
                    let mut line = start.line;
                    while line <= end.line {
                        let col_start =
                            if line == start.line { start.column.0 } else { 0 };
                        let col_end = if line == end.line {
                            end.column.0
                        } else {
                            grid.columns() - 1
                        };
                        for c in col_start..=col_end {
                            s.push(grid[line][Column(c)].c);
                        }
                        line = Line(line.0 + 1);
                    }
                    s
                })
                .collect();
            for (m, uri) in matches.iter().zip(uris_for_matches.into_iter()) {
                let id = self.intern_hyperlink(&uri);
                for line in update.lines.iter_mut() {
                    for col in 0..line.cells.len() {
                        if line.cells[col].flags & FLAG_HYPERLINK != 0 {
                            continue;
                        }
                        let p = viewport_to_point(
                            off,
                            Point::new(line.line as usize, Column(col)),
                        );
                        if point_in_match(p, m) {
                            line.cells[col].flags |= FLAG_HYPERLINK;
                            line.cells[col].hyperlink_id = id;
                        }
                    }
                }
            }
        }
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
        let mut lines: Vec<LineUpdate> = (0..rows)
            .map(|r| LineUpdate {
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
            lines,
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
        }
    }

    pub fn take_damage(&mut self) -> RenderUpdate {
        if self.term.grid().display_offset() > 0 {
            let mut u = self.full_snapshot();
            self.apply_hint_pass(&mut u);
            return u;
        }
        // Collect damaged viewport rows, then drop the borrow before reading cells.
        let damaged: Option<Vec<usize>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(it) => Some(it.map(|b| b.line).collect()),
        };
        self.term.reset_damage();

        let (cursor_line, cursor_col, cursor_visible, cursor_shape, cursor_blinking) =
            self.cursor_fields();
        let mut u = match damaged {
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
                    .map(|row| LineUpdate {
                        line: row as u32,
                        cells: self.line_cells(row),
                    })
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
                }
            }
        };
        self.apply_hint_pass(&mut u);
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
        char::from_u32(line(u, row).cells[col].codepoint).unwrap()
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
        assert_eq!(line(&u, 0).cells[0].bg, 0x00FF_0000, "blank cell uses live bg");
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

    #[test]
    fn writes_plain_text_into_the_grid() {
        let mut e = engine(20, 5);
        e.advance(b"hi".to_vec());
        let u = e.full_snapshot();
        assert_eq!(u.columns(), 20);
        assert_eq!(u.lines.len(), 5);
        assert_eq!(ch(&u, 0, 0), 'h');
        assert_eq!(ch(&u, 0, 1), 'i');
        assert!(u.full);
    }

    #[test]
    fn applies_sgr_foreground_color() {
        let mut e = engine(20, 5);
        e.advance(b"\x1b[31mR".to_vec());
        let u = e.full_snapshot();
        let c = &line(&u, 0).cells[0];
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
        assert_eq!(u.lines.len(), 10);
        assert_eq!(line(&u, 0).cells.len(), 40);
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
                    .cells[0]
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
        assert_eq!(u.lines.len(), 6);
    }

    #[test]
    fn wide_char_sets_wide_and_spacer_flags() {
        let mut e = engine(20, 5);
        e.advance("中".as_bytes().to_vec());
        let u = e.full_snapshot();
        let row0 = line(&u, 0);
        assert_eq!(char::from_u32(row0.cells[0].codepoint).unwrap(), '中');
        assert_ne!(row0.cells[0].flags & FLAG_WIDE, 0, "lead cell must be WIDE_CHAR");
        assert_ne!(
            row0.cells[1].flags & FLAG_WIDE_SPACER,
            0,
            "the cell after a wide char must be WIDE_CHAR_SPACER"
        );
        assert_eq!(
            char::from_u32(row0.cells[1].codepoint).unwrap(),
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
        assert_ne!(line(&e.full_snapshot(), 0).cells[0].flags & FLAG_DIM, 0);

        let mut e2 = engine(20, 5);
        e2.advance(b"\x1b[9mS".to_vec()); // SGR 9 = strikeout
        assert_ne!(
            line(&e2.full_snapshot(), 0).cells[0].flags & FLAG_STRIKEOUT,
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
            .cells.iter().map(|c| char::from_u32(c.codepoint).unwrap()).collect();
        assert!(row0.trim_end().starts_with("line"));
        // Cursor is hidden while scrolled back.
        assert!(!u.cursor_visible);

        e.scroll_to_bottom();
        assert_eq!(e.full_snapshot().display_offset, 0);
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
        assert_ne!(row0.cells[0].flags & FLAG_SELECTED, 0);
        assert_ne!(row0.cells[4].flags & FLAG_SELECTED, 0);
        assert_eq!(row0.cells[10].flags & FLAG_SELECTED, 0); // 'd' not selected

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
            let sel = |c: usize| row0.cells[c].flags & FLAG_SELECTED != 0;
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
        assert_eq!(u.lines[0].cells[0].fg & 0x00FF_FFFF, 0x0011_2233);
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
        assert_ne!(u.lines[0].cells[0].flags & FLAG_MATCH, 0);
        assert_ne!(u.lines[0].cells[0].flags & FLAG_MATCH_CURRENT, 0);
        assert_ne!(u.lines[0].cells[8].flags & FLAG_MATCH, 0);
        assert_eq!(u.lines[0].cells[8].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cells[3].flags & (FLAG_MATCH | FLAG_MATCH_CURRENT), 0);
    }

    #[test]
    fn search_next_moves_focus_to_the_second_match() {
        let mut e = engine(20, 5);
        e.advance(b"foo bar foo".to_vec());
        e.search_set("foo".to_string());
        assert!(e.search_next());
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cells[8].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cells[0].flags & FLAG_MATCH_CURRENT, 0);
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
        assert_ne!(u1.lines[0].cells[8].flags & FLAG_MATCH_CURRENT, 0);
        assert!(e.search_prev());              // back to first "foo"
        let u2 = e.full_snapshot_searched();
        assert_ne!(u2.lines[0].cells[0].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u2.lines[0].cells[8].flags & FLAG_MATCH_CURRENT, 0);
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
        assert_ne!(u.lines[0].cells[0].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cells[3].flags & FLAG_MATCH_CURRENT, 0);

        // next → second.
        assert!(e.search_next());
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cells[0].flags & FLAG_MATCH_CURRENT, 0);
        assert_ne!(u.lines[0].cells[3].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cells[6].flags & FLAG_MATCH_CURRENT, 0);

        // next → third.
        assert!(e.search_next());
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cells[3].flags & FLAG_MATCH_CURRENT, 0);
        assert_ne!(u.lines[0].cells[6].flags & FLAG_MATCH_CURRENT, 0);

        // prev → second.
        assert!(e.search_prev());
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cells[3].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cells[6].flags & FLAG_MATCH_CURRENT, 0);

        // prev → first.
        assert!(e.search_prev());
        let u = e.full_snapshot_searched();
        assert_ne!(u.lines[0].cells[0].flags & FLAG_MATCH_CURRENT, 0);
        assert_eq!(u.lines[0].cells[3].flags & FLAG_MATCH_CURRENT, 0);
    }

    #[test]
    fn invalid_regex_returns_false_and_highlights_nothing() {
        let mut e = engine(20, 5);
        e.advance(b"foo".to_vec());
        assert!(!e.search_set("(".to_string()));
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cells[0].flags & FLAG_MATCH, 0);
    }

    #[test]
    fn search_clear_removes_highlight() {
        let mut e = engine(20, 5);
        e.advance(b"foo".to_vec());
        e.search_set("foo".to_string());
        e.search_clear();
        let u = e.full_snapshot_searched();
        assert_eq!(u.lines[0].cells[0].flags & FLAG_MATCH, 0);
    }

    #[test]
    fn osc8_hyperlink_is_carried_on_cell_data() {
        let mut e = engine(20, 3);
        e.advance(b"\x1b]8;;https://example.com\x1b\\X\x1b]8;;\x1b\\".to_vec());
        let u = e.full_snapshot_searched();
        let cell = &u.lines[0].cells[0];
        assert_ne!(cell.flags & FLAG_HYPERLINK, 0);
        assert_ne!(cell.hyperlink_id, 0);
        assert_eq!(
            e.resolve_hyperlink(cell.hyperlink_id).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn url_auto_detect_marks_visible_region() {
        let mut e = engine(40, 3);
        e.advance(b"see https://x.io/p next".to_vec());
        let u = e.full_snapshot_searched();
        let cell = &u.lines[0].cells[4];
        assert_ne!(cell.flags & FLAG_HYPERLINK, 0);
        assert_ne!(cell.hyperlink_id, 0);
        let uri = e.resolve_hyperlink(cell.hyperlink_id).unwrap();
        assert!(uri.starts_with("https://x.io/p"), "uri was {uri:?}");
        assert_eq!(u.lines[0].cells[2].flags & FLAG_HYPERLINK, 0);
    }

    #[test]
    fn url_auto_detect_applies_on_take_damage_path() {
        let mut e = engine(40, 3);
        e.take_damage(); // drain initial full damage
        e.advance(b"see https://x.io/p next".to_vec());
        let u = e.take_damage();
        let row = u.lines.iter().find(|l| l.line == 0).expect("row 0");
        let cell = &row.cells[4];
        assert_ne!(cell.flags & FLAG_HYPERLINK, 0);
        assert_ne!(cell.hyperlink_id, 0);
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
    fn osc8_wins_over_auto_detect_when_both_apply() {
        let mut e = engine(40, 3);
        e.advance(
            b"\x1b]8;;https://osc8.example\x1b\\https://other.example\x1b]8;;\x1b\\".to_vec(),
        );
        let u = e.full_snapshot_searched();
        let cell = &u.lines[0].cells[0];
        assert_eq!(
            e.resolve_hyperlink(cell.hyperlink_id).as_deref(),
            Some("https://osc8.example")
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
}
