pub use crate::engine::{CellData, EngineConfig, LineUpdate, RenderUpdate, TerminalEngine};
pub use crate::event_proxy::EngineEvent;

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
pub fn engine_full_snapshot(engine: &TerminalEngine) -> RenderUpdate {
    engine.full_snapshot()
}

#[frb(sync)]
pub fn engine_resize(engine: &mut TerminalEngine, columns: u16, rows: u16) {
    engine.resize(columns, rows);
}

pub async fn engine_scroll_lines(engine: &mut TerminalEngine, delta: i32) {
    engine.scroll_lines(delta);
}

pub async fn engine_scroll_to_bottom(engine: &mut TerminalEngine) {
    engine.scroll_to_bottom();
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
pub fn engine_search_set(engine: &mut TerminalEngine, pattern: String) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.search_set(pattern))).unwrap_or(false)
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
pub fn engine_full_snapshot_searched(engine: &mut TerminalEngine) -> RenderUpdate {
    std::panic::catch_unwind(AssertUnwindSafe(|| engine.full_snapshot_searched()))
        .unwrap_or_else(|_| engine.full_snapshot())
}
