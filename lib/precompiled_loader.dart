/// Downloads and verifies Cargokit precompiled Rust binaries.
library;

import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:collection/collection.dart';
import 'package:convert/convert.dart';
import 'package:crypto/crypto.dart';
import 'package:ed25519_edwards/ed25519_edwards.dart';
import 'package:http/http.dart' as http;
import 'package:path/path.dart' as p;
import 'package:yaml/yaml.dart';

import 'src/plugin_paths.dart';

const _libraryName = 'rust_lib_flutter_alacritty';

/// Rust cdylib filename for the current OS.
String precompiledRustLibFileName() {
  if (Platform.isWindows) return '$_libraryName.dll';
  if (Platform.isMacOS) return 'lib$_libraryName.dylib';
  return 'lib$_libraryName.so';
}

/// Host triple used for Cargokit precompiled artifact names.
String hostRustTriple() {
  switch (Abi.current()) {
    case Abi.windowsX64:
      return 'x86_64-pc-windows-msvc';
    case Abi.windowsArm64:
      return 'aarch64-pc-windows-msvc';
    case Abi.macosX64:
      return 'x86_64-apple-darwin';
    case Abi.macosArm64:
      return 'aarch64-apple-darwin';
    case Abi.linuxX64:
      return 'x86_64-unknown-linux-gnu';
    case Abi.linuxArm64:
      return 'aarch64-unknown-linux-gnu';
    default:
      throw UnsupportedError(
        'Precompiled rust_lib_flutter_alacritty is only supported on desktop '
        '(host ABI: ${Abi.current()})',
      );
  }
}

/// Cargokit remote artifact file name: `{triple}_{library-file}`.
String precompiledRemoteFileName(String rustTriple, String libraryFile) {
  return '${rustTriple}_$libraryFile';
}

/// Downloads a verified release cdylib into [cacheDir] and returns its path.
///
/// Returns `null` when precompiled binaries are not configured, not published
/// for the current crate hash / platform, or signature verification fails.
Future<String?> downloadPrecompiledRustLib({
  String? manifestDir,
  String? cacheDir,
}) async {
  final manifest = manifestDir ?? rustManifestDir();
  final config = _loadPrecompiledConfig(manifest);
  if (config == null) return null;

  final crateHash = _computeCrateHash(manifest);
  final triple = hostRustTriple();
  final libraryFile = precompiledRustLibFileName();
  final remoteName = precompiledRemoteFileName(triple, libraryFile);
  final signatureName = '$remoteName.sig';

  final store = cacheDir ??
      p.join(
        Directory.systemTemp.path,
        'rust_lib_flutter_alacritty_precompiled',
        crateHash,
      );
  Directory(store).createSync(recursive: true);
  final localPath = p.join(store, libraryFile);
  if (File(localPath).existsSync()) {
    return localPath;
  }

  final prefix = config.urlPrefix;
  final binaryUrl = Uri.parse('$prefix$crateHash/$remoteName');
  final signatureUrl = Uri.parse('$prefix$crateHash/$signatureName');

  final signatureResponse = await http.get(signatureUrl);
  if (signatureResponse.statusCode != 200) {
    return null;
  }

  final binaryResponse = await http.get(binaryUrl);
  if (binaryResponse.statusCode != 200) {
    return null;
  }

  if (!verify(
    config.publicKey,
    binaryResponse.bodyBytes,
    signatureResponse.bodyBytes,
  )) {
    return null;
  }

  await File(localPath).writeAsBytes(binaryResponse.bodyBytes);
  return localPath;
}

class _PrecompiledConfig {
  _PrecompiledConfig({required this.urlPrefix, required this.publicKey});

  final String urlPrefix;
  final PublicKey publicKey;
}

_PrecompiledConfig? _loadPrecompiledConfig(String manifestDir) {
  final file = File(p.join(manifestDir, 'cargokit.yaml'));
  if (!file.existsSync()) return null;

  final root = loadYaml(file.readAsStringSync());
  if (root is! YamlMap) return null;

  final section = root['precompiled_binaries'];
  if (section is! YamlMap) return null;

  final urlPrefix = section['url_prefix'];
  final publicKeyHex = section['public_key'];
  if (urlPrefix is! String || publicKeyHex is! String) return null;

  final keyBytes = hex.decode(publicKeyHex);
  if (keyBytes.length != 32) return null;

  return _PrecompiledConfig(
    urlPrefix: urlPrefix,
    publicKey: PublicKey(keyBytes),
  );
}

/// Mirrors Cargokit `CrateHash.compute` so download URLs stay in sync.
String _computeCrateHash(String manifestDir) {
  final files = _crateHashFiles(manifestDir);
  final output = AccumulatorSink<Digest>();
  final input = sha256.startChunkedConversion(output);
  const splitter = LineSplitter();

  for (final file in files) {
    if (file.existsSync()) {
      for (final line in splitter.convert(file.readAsStringSync())) {
        input.add(utf8.encode(line));
      }
    }
  }

  input.close();
  return hex.encode(output.events.single.bytes.sublist(0, 16));
}

List<File> _crateHashFiles(String manifestDir) {
  final src = Directory(p.join(manifestDir, 'src'));
  final files = src
      .listSync(recursive: true, followLinks: false)
      .whereType<File>()
      .toList();
  files.sortBy((element) => element.path);

  for (final relative in ['Cargo.toml', 'Cargo.lock', 'build.rs', 'cargokit.yaml']) {
    final file = File(p.join(manifestDir, relative));
    if (file.existsSync()) {
      files.add(file);
    }
  }

  return files;
}
