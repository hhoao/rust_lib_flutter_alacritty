## 0.2.2

- Pin `alacritty_terminal` to fork rev with
  `RegexSearch::with_case_insensitive` (no local path patch).
- Search options FFI (case / whole word / regex / wrap).
- DEC 2026 synchronized-output mode bit tracking.
- Tighter `scroll_fraction` damage; Rust-raster present path;
  preserve selection/search flags on partial `take_damage`.

## 0.2.1

- Add Cargokit precompiled-binary support (`rust/cargokit.yaml`).
- GitHub Actions workflow uploads signed release artifacts for Linux, macOS, and
  Windows; macOS cdylibs are also published for Dart/widget tests.
- Export `precompiled_loader.dart` for downloading verified binaries in tests.
- Document setup in `docs/PRECOMPILED_BINARIES.md`.

## 0.2.0

- Runtime palette and terminal option reconfiguration; OSC 997 color-scheme
  reporting for DEC mode 2031.
- Sub-cell pixel scroll with overscan row; `scroll_to_offset`, `history_size`,
  and incremental `scroll_refresh` edge-row damage.
- ViewFrame damage protocol; columnar `LineUpdate` FFI (drops per-cell Dart
  marshaling).
- OSC pre-parser for cwd (OSC 7) and desktop notifications (OSC 9 / 777).
- VT minimum grid clamp; OSC 8 hyperlinks only (URL hint pass removed).
- Wide-glyph selection fix, `clear_history` FFI, OSC 52 paste, and mouse-mode
  cursor updates.

## 0.1.0

- Initial pub.dev release of the Rust FFI plugin for `flutter_alacritty`.
- Bundles the `rust/` crate (Alacritty terminal engine) and Cargokit build.
