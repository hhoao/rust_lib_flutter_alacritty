import 'dart:io';

import 'package:ed25519_edwards/ed25519_edwards.dart';
import 'package:hex/hex.dart';

import '../lib/src/options.dart';

/// Ensures PRIVATE_KEY matches public_key in rust/cargokit.yaml before upload.
Future<void> main(List<String> args) async {
  final manifestDir = args.isNotEmpty ? args.first : '../../rust';
  final privateHex = Platform.environment['PRIVATE_KEY'];
  if (privateHex == null) {
    stderr.writeln('Missing PRIVATE_KEY');
    exit(1);
  }

  final privateBytes = HEX.decode(privateHex);
  if (privateBytes.length != 64) {
    stderr.writeln('PRIVATE_KEY must be 64 bytes (128 hex chars)');
    exit(1);
  }

  final options = CargokitCrateOptions.load(manifestDir: manifestDir);
  final configured = options.precompiledBinaries;
  if (configured == null) {
    stderr.writeln('No precompiled_binaries in cargokit.yaml');
    exit(1);
  }

  final derivedPublic = HEX.encode(public(PrivateKey(privateBytes)).bytes);
  final configuredPublic = HEX.encode(configured.publicKey.bytes);
  if (derivedPublic != configuredPublic) {
    stderr.writeln('RELEASE_PRIVATE_KEY does not match public_key in cargokit.yaml');
    stderr.writeln('  cargokit.yaml public_key: $configuredPublic');
    stderr.writeln('  derived from PRIVATE_KEY: $derivedPublic');
    stderr.writeln('Re-run `dart run build_tool gen-key` and update BOTH files/secrets.');
    exit(1);
  }

  stdout.writeln('Key pair OK ($configuredPublic)');
}
