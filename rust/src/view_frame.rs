/// Sentinel `viewport_row` for overscan lines (partial scroll with fractional offset).
pub const OVERSCAN_LINE_TAG: u32 = u32::MAX;

/// Per-frame chrome: cursor, mode, scroll state, and default colors.
#[derive(Clone, Debug)]
pub struct ViewChrome {
    pub cursor_line: u32,
    pub cursor_col: u32,
    pub cursor_visible: bool,
    pub cursor_shape: u8,
    pub cursor_blinking: bool,
    pub mode_flags: u32,
    pub display_offset: u32,
    pub history_size: u32,
    pub scroll_fraction: f64,
    /// Viewport ring rotation hint; 0 = none.
    pub scroll_rotate: i32,
    pub default_fg: u32,
    pub default_bg: u32,
    pub cursor_color: u32,
}

/// Column-range damage for one viewport row.
///
/// Column semantics match Alacritty [`LineDamageBounds`](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/term/mod.rs):
/// `start_col` and `end_col` are both inclusive (`left..=right`).
#[derive(Clone, Debug)]
pub struct RowDamage {
    /// 0..rows-1, or [`OVERSCAN_LINE_TAG`] for overscan.
    pub viewport_row: u32,
    pub start_col: u16,
    pub end_col: u16,
    pub codepoints: Vec<u32>,
    pub fg: Vec<u32>,
    pub bg: Vec<u32>,
    pub flags: Vec<u16>,
    pub hyperlink_id: Vec<u32>,
}

/// Search match highlight span in viewport coordinates.
#[derive(Clone, Debug)]
pub struct SearchSpan {
    pub viewport_row: u32,
    pub start_col: u16,
    pub end_col: u16,
    pub is_current: bool,
}

/// Single damage-driven view update emitted per engine mutation.
#[derive(Clone, Debug)]
pub struct ViewFrame {
    pub chrome: ViewChrome,
    pub damage: Vec<RowDamage>,
    pub search_spans: Vec<SearchSpan>,
    /// Resize / explicit invalidate — painter must scan all rows.
    pub full_viewport: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_damage_column_count_matches_range() {
        let d = RowDamage {
            viewport_row: 0,
            start_col: 2,
            end_col: 5,
            codepoints: vec![65, 66, 67, 68],
            fg: vec![0; 4],
            bg: vec![0; 4],
            flags: vec![0; 4],
            hyperlink_id: vec![0; 4],
        };
        assert_eq!(d.codepoints.len(), (d.end_col - d.start_col + 1) as usize);
    }
}
