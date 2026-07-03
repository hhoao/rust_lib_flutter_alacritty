# Precompiled Rust binaries

`rust_lib_flutter_alacritty` uses [Cargokit](https://github.com/irondash/cargokit) to
integrate the Rust `cdylib` with Flutter. By default, **consumers without Rust**
download signed release binaries during `flutter build`; consumers with Rustup
compile locally unless they opt in to precompiled artifacts.

Configuration lives in [`rust/cargokit.yaml`](../rust/cargokit.yaml).

## Consumer setup (no Rust required)

1. Add `flutter_alacritty` (which pulls in this plugin) as usual.
2. Run `flutter pub get` and `flutter build` / `flutter run`.

On first native build, Cargokit downloads the binary matching the crate hash and
your target platform from GitHub Releases, verifies the Ed25519 signature, and
bundles it into the app.

### Force precompiled binaries (optional)

Create `cargokit_options.yaml` in your app root:

```yaml
use_precompiled_binaries: true
```

Useful for faster CI when Rust is installed but you still want release artifacts.

### Widget / unit tests

Import the shared test helper from `flutter_alacritty`:

```dart
import 'package:flutter_alacritty/testing/rust_lib_loader.dart';

setUpAll(() async {
  await initRustLibForTests();
});
```

It tries local `cargo` output, then downloads the same signed release binaries.

## Maintainer setup (one-time)

### 1. Generate a signing key pair

```bash
cd cargokit/build_tool
dart pub get
dart run build_tool gen-key
```

- Put the **public** key in `rust/cargokit.yaml`.
- Store the **private** key as the `RELEASE_PRIVATE_KEY` repository secret
  (64-byte hex string).

### 2. GitHub token

Create a fine-grained or classic token with `contents: write` on this repo and
add it as `RELEASE_GITHUB_TOKEN`.

### 3. CI

[`.github/workflows/precompile-binaries.yml`](../.github/workflows/precompile-binaries.yml)
runs on every push to `main` that touches `rust/` (and on manual dispatch). It:

1. Builds all targets available on each host (Linux / macOS / Windows).
2. Uploads signed artifacts to `precompiled_<crate-hash>` releases.
3. Uploads extra macOS `cdylib` files for Dart tests.
4. Verifies all published artifacts.

### 4. Publish to pub.dev

Bump the plugin version, update `CHANGELOG.md`, and `dart pub publish` **after**
CI has uploaded binaries for the new crate hash (push to `main` first).

## Crate hash

The download URL is:

```
{url_prefix}{crate-hash}/{target-triple}_{artifact-name}
```

`crate-hash` is a truncated SHA-256 of all `rust/src/**/*.rs`, `Cargo.toml`,
`Cargo.lock`, `build.rs`, and `cargokit.yaml`. Any Rust change requires a new
CI run before consumers without Rust can build.

Check the current hash locally:

```bash
cd cargokit/build_tool
dart run build_tool verify-binaries --manifest-dir=../../rust
```
