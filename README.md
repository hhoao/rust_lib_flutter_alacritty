# rust_lib_flutter_alacritty

FFI plugin for [`flutter_alacritty`](https://github.com/hhoao/flutter_alacritty).
Builds the Alacritty-based terminal engine (Rust `cdylib`) via Cargokit.

Standalone repository: [hhoao/rust_lib_flutter_alacritty](https://github.com/hhoao/rust_lib_flutter_alacritty)

You normally do **not** depend on this package directly; add `flutter_alacritty`
instead.

## Layout

- `rust/` — Rust crate (`rust_lib_flutter_alacritty`) + `cargokit.yaml`
- `cargokit/` — native build integration
- `lib/precompiled_loader.dart` — download verified release binaries (tests)
- Platform folders — Android, iOS, Linux, macOS, Windows plugin glue

## Alacritty terminal dependency

`rust/Cargo.toml` pins `alacritty_terminal` to
[hhoao/alacritty](https://github.com/hhoao/alacritty) at a `rev` that includes
`RegexSearch::with_case_insensitive`. Switch back to upstream once that API
merges. For local hacks, add a temporary `[patch]` path override (do not
commit it).

## Precompiled binaries

Consumers without Rust download signed artifacts during `flutter build`. See
[docs/PRECOMPILED_BINARIES.md](docs/PRECOMPILED_BINARIES.md) for maintainer CI
setup (`RELEASE_GITHUB_TOKEN`, `RELEASE_PRIVATE_KEY` secrets).

## License

MIT — see [LICENSE](LICENSE).
