/// Uploads macOS release cdylibs for Dart/widget tests.
///
/// Cargokit precompile publishes static libraries for Apple targets (linked at
/// app build time). Tests load a dynamic library via `ExternalLibrary.open`, so
/// this tool uploads signed `*.dylib` artifacts to the same release tag.
import 'dart:io';

import 'package:args/args.dart';
import 'package:ed25519_edwards/ed25519_edwards.dart';
import 'package:github/github.dart';
import 'package:hex/hex.dart';
import 'package:logging/logging.dart';
import 'package:path/path.dart' as path;

import '../lib/src/artifacts_provider.dart';
import '../lib/src/cargo.dart';
import '../lib/src/crate_hash.dart';
import '../lib/src/logging.dart';
import '../lib/src/options.dart';
import '../lib/src/precompile_binaries.dart';
import '../lib/src/rustup.dart';
import '../lib/src/target.dart';
import '../lib/src/builder.dart';

Future<void> main(List<String> args) async {
  initLogging();
  final parser = ArgParser()
    ..addOption(
      'manifest-dir',
      mandatory: true,
      help: 'Directory containing Cargo.toml',
    )
    ..addOption(
      'repository',
      mandatory: true,
      help: 'GitHub repository slug (owner/name)',
    )
    ..addFlag('verbose', abbr: 'v', defaultsTo: false);

  final results = parser.parse(args);
  if (results['verbose'] as bool) {
    enableVerboseLogging();
  }

  final privateKeyString = Platform.environment['PRIVATE_KEY'];
  final githubToken = Platform.environment['GITHUB_TOKEN'];
  if (privateKeyString == null) {
    stderr.writeln('Missing PRIVATE_KEY environment variable');
    exit(1);
  }
  if (githubToken == null) {
    stderr.writeln('Missing GITHUB_TOKEN environment variable');
    exit(1);
  }

  final privateKeyBytes = HEX.decode(privateKeyString);
  if (privateKeyBytes.length != 64) {
    stderr.writeln('PRIVATE_KEY must be 64 bytes (hex-encoded)');
    exit(1);
  }
  final privateKey = PrivateKey(privateKeyBytes);

  final manifestDir = results['manifest-dir'] as String;
  final repository = RepositorySlug.full(results['repository'] as String);
  final log = Logger('upload_macos_cdylib');

  if (!Platform.isMacOS) {
    log.warning('Skipping macOS cdylib upload on non-macOS host');
    return;
  }

  final crateInfo = CrateInfo.load(manifestDir);
  final crateOptions = CargokitCrateOptions.load(manifestDir: manifestDir);
  if (crateOptions.precompiledBinaries == null) {
    stderr.writeln('cargokit.yaml does not configure precompiled_binaries');
    exit(1);
  }

  final hash = CrateHash.compute(manifestDir);
  final tagName = 'precompiled_$hash';
  log.info('Crate hash: $hash (release tag: $tagName)');

  final tempDir = Directory.systemTemp.createTempSync('macos_cdylib_');
  try {
    final buildEnvironment = BuildEnvironment(
      configuration: BuildConfiguration.release,
      crateOptions: crateOptions,
      targetTempDir: tempDir.path,
      manifestDir: manifestDir,
      crateInfo: crateInfo,
      isAndroid: false,
    );

    final rustup = Rustup();
    final darwinTargets = Target.buildableTargets()
        .where((t) => t.darwinPlatform != null)
        .toList(growable: false);

    final github = GitHub(auth: Authentication.withToken(githubToken));
    final repo = github.repositories;

    Release release;
    try {
      release = await repo.getReleaseByTagName(repository, tagName);
    } on ReleaseNotFound {
      stderr.writeln(
        'Release $tagName not found. Run precompile-binaries first.',
      );
      exit(1);
    }

    for (final target in darwinTargets) {
      final artifactNames = getArtifactNames(
        target: target,
        libraryName: crateInfo.packageName,
        remote: true,
        aritifactType: AritifactType.dylib,
      );

      final fileName = PrecompileBinaries.fileName(target, artifactNames.single);
      if ((release.assets ?? []).any((asset) => asset.name == fileName)) {
        log.info('$fileName already published — skipping');
        continue;
      }

      log.info('Building cdylib for $target');
      final builder = RustBuilder(target: target, environment: buildEnvironment);
      builder.prepare(rustup);
      final outDir = await builder.build();
      final dylib = File(path.join(outDir, artifactNames.single));
      if (!dylib.existsSync()) {
        stderr.writeln('Missing dylib at ${dylib.path}');
        exit(1);
      }

      final data = dylib.readAsBytesSync();
      final signature = sign(privateKey, data);
      if (!verify(public(privateKey), data, signature)) {
        stderr.writeln('Signature self-check failed for $fileName');
        exit(1);
      }

      final assets = [
        CreateReleaseAsset(
          name: fileName,
          contentType: 'application/octet-stream',
          assetData: data,
        ),
        CreateReleaseAsset(
          name: PrecompileBinaries.signatureFileName(
            target,
            artifactNames.single,
          ),
          contentType: 'application/octet-stream',
          assetData: signature,
        ),
      ];

      log.info('Uploading $fileName');
      await repo.uploadReleaseAssets(release, assets);
    }
  } finally {
    tempDir.deleteSync(recursive: true);
  }
}
