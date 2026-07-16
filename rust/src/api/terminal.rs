pub use crate::engine::{CellData, EngineConfig, LineUpdate, RenderUpdate, SearchOptions, TerminalEngine};
pub use crate::event_proxy::EngineEvent;
pub use crate::terminal_raster_present::RasterPresentFrame;

use flutter_rust_bridge::frb;
use std::panic::AssertUnwindSafe;

#[frb(sync)]
pub fn engine_new(columns: u16, rows: u16, config: EngineConfig) -> TerminalEngine {
    TerminalEngine::new(columns, rows, config)
}

pub async fn engine_advance(engine: &mut TerminalEngine, bytes: Vec<u8>) {
    // §6 panic isolation: a malformed sequence must never abort the app.
    if std::panic::catch_unwind(AssertUnwindSafe(|| engine.advance(bytes))).is_err() {
        eprintln!("flutter_alacritty: engine_advance panicked (input discarded)");
    }
}

pub async fn engine_take_damage(engine: &mut TerminalEngine) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.take_damage())).unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_take_damage panicked (empty update returned)");
        RenderUpdate {
            lines: Vec::new(),
            full: false,
            cursor_line: 0,
            cursor_col: 0,
            cursor_visible: false,
            cursor_shape: 0,
            cursor_blinking: false,
            mode_flags: 0,
            display_offset: 0,
            history_size: 0,
            default_fg: crate::engine::EngineConfig::default_palette()[16],
            default_bg: crate::engine::EngineConfig::default_palette()[17],
            cursor_color: crate::engine::CURSOR_COLOR_UNSET,
            scroll_fraction: 0.0,
            scroll_line_delta: 0,
        }
    })
}

/// Single FFI round-trip: parse PTY bytes then return damage (hot path).
pub async fn engine_advance_and_take_damage(
    engine: &mut TerminalEngine,
    bytes: Vec<u8>,
) -> RenderUpdate {
    if std::panic::catch_unwind(AssertUnwindSafe(|| engine.advance(bytes))).is_err() {
        eprintln!(
            "flutter_alacritty: engine_advance_and_take_damage advance panicked (input discarded)"
        );
    }
    engine_take_damage(engine).await
}

#[frb(sync)]
pub fn engine_take_events(engine: &TerminalEngine) -> Vec<EngineEvent> {
    engine.take_events()
}

#[frb(sync)]
pub fn engine_full_snapshot(engine: &mut TerminalEngine) -> RenderUpdate {
    engine.full_snapshot()
}

#[frb(sync)]
pub fn engine_resize(engine: &mut TerminalEngine, columns: u16, rows: u16) {
    engine.resize(columns, rows);
}

pub async fn engine_scroll_lines(engine: &mut TerminalEngine, delta: i32) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.scroll_lines(delta))).unwrap_or_else(
        |_| {
            eprintln!("flutter_alacritty: engine_scroll_lines panicked (empty update returned)");
            RenderUpdate {
                lines: Vec::new(),
                full: false,
                cursor_line: 0,
                cursor_col: 0,
                cursor_visible: false,
                cursor_shape: 0,
                cursor_blinking: false,
                mode_flags: 0,
                display_offset: 0,
                history_size: 0,
                default_fg: crate::engine::EngineConfig::default_palette()[16],
                default_bg: crate::engine::EngineConfig::default_palette()[17],
                cursor_color: crate::engine::CURSOR_COLOR_UNSET,
                scroll_fraction: 0.0,
                scroll_line_delta: 0,
            }
        },
    )
}

/// Sub-cell pixel scroll. Positive `delta_px` scrolls up into history.
pub async fn engine_scroll_pixels(engine: &mut TerminalEngine, delta_px: f64) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.scroll_pixels(delta_px))).unwrap_or_else(
        |_| {
            eprintln!("flutter_alacritty: engine_scroll_pixels panicked (empty update returned)");
            RenderUpdate {
                lines: Vec::new(),
                full: false,
                cursor_line: 0,
                cursor_col: 0,
                cursor_visible: false,
                cursor_shape: 0,
                cursor_blinking: false,
                mode_flags: 0,
                display_offset: 0,
                history_size: 0,
                default_fg: crate::engine::EngineConfig::default_palette()[16],
                default_bg: crate::engine::EngineConfig::default_palette()[17],
                cursor_color: crate::engine::CURSOR_COLOR_UNSET,
                scroll_fraction: 0.0,
                scroll_line_delta: 0,
            }
        },
    )
}

pub async fn engine_scroll_to_bottom(engine: &mut TerminalEngine) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.scroll_to_bottom())).unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_to_bottom panicked (empty update returned)");
        RenderUpdate {
            lines: Vec::new(),
            full: false,
            cursor_line: 0,
            cursor_col: 0,
            cursor_visible: false,
            cursor_shape: 0,
            cursor_blinking: false,
            mode_flags: 0,
            display_offset: 0,
            history_size: 0,
            default_fg: crate::engine::EngineConfig::default_palette()[16],
            default_bg: crate::engine::EngineConfig::default_palette()[17],
            cursor_color: crate::engine::CURSOR_COLOR_UNSET,
            scroll_fraction: 0.0,
            scroll_line_delta: 0,
        }
    })
}

pub async fn engine_scroll_to_top(engine: &mut TerminalEngine) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.scroll_to_top())).unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_to_top panicked (empty update returned)");
        RenderUpdate {
            lines: Vec::new(),
            full: false,
            cursor_line: 0,
            cursor_col: 0,
            cursor_visible: false,
            cursor_shape: 0,
            cursor_blinking: false,
            mode_flags: 0,
            display_offset: 0,
            history_size: 0,
            default_fg: crate::engine::EngineConfig::default_palette()[16],
            default_bg: crate::engine::EngineConfig::default_palette()[17],
            cursor_color: crate::engine::CURSOR_COLOR_UNSET,
            scroll_fraction: 0.0,
            scroll_line_delta: 0,
        }
    })
}

/// Absolute scroll position in lines (`0` = live bottom, `history_size` = top).
pub async fn engine_scroll_to_offset(
    engine: &mut TerminalEngine,
    offset_lines: f64,
) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.scroll_to_offset(offset_lines)))
        .unwrap_or_else(|_| {
            eprintln!(
                "flutter_alacritty: engine_scroll_to_offset panicked (empty update returned)"
            );
            RenderUpdate {
                lines: Vec::new(),
                full: false,
                cursor_line: 0,
                cursor_col: 0,
                cursor_visible: false,
                cursor_shape: 0,
                cursor_blinking: false,
            mode_flags: 0,
            display_offset: 0,
                history_size: 0,
                default_fg: crate::engine::EngineConfig::default_palette()[16],
                default_bg: crate::engine::EngineConfig::default_palette()[17],
                cursor_color: crate::engine::CURSOR_COLOR_UNSET,
                scroll_fraction: 0.0,
                scroll_line_delta: 0,
            }
        })
}

#[frb(sync)]
pub fn engine_clear_history(engine: &mut TerminalEngine) {
    engine.clear_history();
}

#[frb(sync)]
pub fn engine_selection_start(
    engine: &mut TerminalEngine,
    display_row: i32,
    col: u16,
    right_half: bool,
    kind: u8,
) {
    engine.selection_start(display_row, col, right_half, kind);
}

#[frb(sync)]
pub fn engine_selection_update(
    engine: &mut TerminalEngine,
    display_row: i32,
    col: u16,
    right_half: bool,
) {
    engine.selection_update(display_row, col, right_half);
}

#[frb(sync)]
pub fn engine_selection_clear(engine: &mut TerminalEngine) {
    engine.selection_clear();
}

#[frb(sync)]
pub fn engine_selection_text(engine: &TerminalEngine) -> Option<String> {
    engine.selection_text()
}

#[frb(sync)]
pub fn engine_search_set(
    engine: &mut TerminalEngine,
    pattern: String,
    options: SearchOptions,
) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.search_set(pattern, options)))
        .unwrap_or(false)
}

#[frb(sync)]
pub fn engine_search_next(engine: &mut TerminalEngine) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.search_next())).unwrap_or(false)
}

#[frb(sync)]
pub fn engine_search_prev(engine: &mut TerminalEngine) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.search_prev())).unwrap_or(false)
}

#[frb(sync)]
pub fn engine_search_clear(engine: &mut TerminalEngine) {
    engine.search_clear();
}

#[frb(sync)]
pub fn engine_search_is_active(engine: &TerminalEngine) -> bool {
    engine.search_is_active()
}

#[frb(sync)]
pub fn engine_resolve_hyperlink(engine: &TerminalEngine, id: u32) -> Option<String> {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.resolve_hyperlink(id)))
        .unwrap_or(None)
}

#[frb(sync)]
pub fn engine_full_snapshot_searched(engine: &mut TerminalEngine) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.full_snapshot_searched()))
        .unwrap_or_else(|_| engine.full_snapshot())
}

#[frb(sync)]
pub fn engine_respond_clipboard_load(engine: &mut TerminalEngine, text: String) {
    engine.respond_clipboard_load(text);
}

#[frb(sync)]
pub fn engine_set_cell_pixels(engine: &mut TerminalEngine, width: u16, height: u16) {
    engine.set_cell_pixels(width, height);
}

#[frb(sync)]
pub fn engine_reconfigure(engine: &mut TerminalEngine, config: EngineConfig) {
    engine.reconfigure(config);
}

/// Rust-raster present: consume damage and return RGBA + chrome (no LineUpdate
/// mirror payload). Dart uploads `rgba` to a retained `ui.Image`.
#[frb(sync)]
pub fn engine_take_raster_present(engine: &mut TerminalEngine) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.take_raster_present())).unwrap_or_else(
        |_| {
            eprintln!("flutter_alacritty: engine_take_raster_present panicked");
            empty_raster_frame()
        },
    )
}

/// Full viewport Rust-raster present (resize / explicit refresh).
#[frb(sync)]
pub fn engine_full_raster_present(engine: &mut TerminalEngine) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.full_raster_present())).unwrap_or_else(
        |_| {
            eprintln!("flutter_alacritty: engine_full_raster_present panicked");
            empty_raster_frame()
        },
    )
}

/// Single FFI round-trip: parse PTY bytes then raster-present.
pub async fn engine_advance_and_take_raster_present(
    engine: &mut TerminalEngine,
    bytes: Vec<u8>,
) -> RasterPresentFrame {
    if std::panic::catch_unwind(AssertUnwindSafe(|| engine.advance(bytes))).is_err() {
        eprintln!(
            "flutter_alacritty: engine_advance_and_take_raster_present advance panicked"
        );
    }
    engine_take_raster_present(engine)
}

pub async fn engine_scroll_lines_raster(
    engine: &mut TerminalEngine,
    delta: i32,
) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let u = engine.scroll_lines(delta);
        engine.rasterize_update(&u)
    }))
    .unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_lines_raster panicked");
        empty_raster_frame()
    })
}

pub async fn engine_scroll_pixels_raster(
    engine: &mut TerminalEngine,
    delta_px: f64,
) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let u = engine.scroll_pixels(delta_px);
        engine.rasterize_update(&u)
    }))
    .unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_pixels_raster panicked");
        empty_raster_frame()
    })
}

pub async fn engine_scroll_to_bottom_raster(
    engine: &mut TerminalEngine,
) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let u = engine.scroll_to_bottom();
        engine.rasterize_update(&u)
    }))
    .unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_to_bottom_raster panicked");
        empty_raster_frame()
    })
}

pub async fn engine_scroll_to_top_raster(engine: &mut TerminalEngine) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let u = engine.scroll_to_top();
        engine.rasterize_update(&u)
    }))
    .unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_to_top_raster panicked");
        empty_raster_frame()
    })
}

pub async fn engine_scroll_to_offset_raster(
    engine: &mut TerminalEngine,
    offset_lines: f64,
) -> RasterPresentFrame {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let u = engine.scroll_to_offset(offset_lines);
        engine.rasterize_update(&u)
    }))
    .unwrap_or_else(|_| {
        eprintln!("flutter_alacritty: engine_scroll_to_offset_raster panicked");
        empty_raster_frame()
    })
}

fn empty_raster_frame() -> RasterPresentFrame {
    RasterPresentFrame {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 0, 255],
        cursor_line: 0,
        cursor_col: 0,
        cursor_visible: false,
        cursor_shape: 0,
        cursor_blinking: false,
        mode_flags: 0,
        display_offset: 0,
        history_size: 0,
        scroll_fraction: 0.0,
        default_fg: crate::engine::EngineConfig::default_palette()[16],
        default_bg: crate::engine::EngineConfig::default_palette()[17],
        cursor_color: crate::engine::CURSOR_COLOR_UNSET,
        full: true,
    }
}

