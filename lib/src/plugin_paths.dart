/// Resolves the on-disk root of the `rust_lib_flutter_alacritty` package.
library;

import 'dart:io';

import 'package:path/path.dart' as p;

/// Absolute path to the plugin package root (parent of `lib/` and `rust/`).
String rustLibPluginRoot() {
  return p.normalize(p.join(p.dirname(p.fromUri(Platform.script)), '..'));
}

/// Absolute path to the Rust manifest directory (`Cargo.toml`, `cargokit.yaml`).
String rustManifestDir() => p.join(rustLibPluginRoot(), 'rust');
