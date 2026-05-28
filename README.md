# rust_lib_flutter_alacritty

FFI plugin for [`flutter_alacritty`](https://github.com/hhoao/flutter_alacritty).
Builds the Alacritty-based terminal engine (Rust `cdylib`) via Cargokit.

Standalone repository: [hhoao/rust_lib_flutter_alacritty](https://github.com/hhoao/rust_lib_flutter_alacritty)

You normally do **not** depend on this package directly; add `flutter_alacritty`
instead.

## Layout

- `rust/` — Rust crate (`rust_lib_flutter_alacritty`)
- `cargokit/` — native build integration
- Platform folders — Android, iOS, Linux, macOS, Windows plugin glue

## License

MIT — see [LICENSE](LICENSE).
