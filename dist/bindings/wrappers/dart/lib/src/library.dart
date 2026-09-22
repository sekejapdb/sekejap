// Finding `libsekejap` at run time.
//
// The wrapper builds no native code: the library is the `sekejap-capi` crate
// (`dist/ffi`), built once and shipped as `libsekejap.{dylib,so,dll}`. This
// file is the whole of "where is it", in one documented order.

import 'dart:ffi';
import 'dart:io';

import 'bindings.dart';
import 'status.dart';

SekejapBindings? _bindings;
String? _explicitPath;

/// Load `libsekejap` from [path] instead of searching for it.
///
/// Call this before the first database call. Calling it after the library is
/// already loaded throws [StateError]: one process loads one library.
void useSekejapLibrary(String path) {
  if (_bindings != null) {
    throw StateError(
        'libsekejap is already loaded; useSekejapLibrary must be called '
        'before the first database call.');
  }
  _explicitPath = path;
}

/// Whether the library has been loaded into this process yet.
bool get sekejapLibraryLoaded => _bindings != null;

/// The bound C entry points, loading the library on first use.
///
/// The search order, first hit wins:
///
/// 1. the path given to [useSekejapLibrary];
/// 2. the `SEKEJAP_LIBRARY` environment variable (a full path to the file);
/// 3. the running process, when it already carries `sekejap_version` -- a
///    Flutter app whose plugin bundled the library, or a host that linked
///    `libsekejap.a`;
/// 4. the platform's plain library name (`libsekejap.dylib`, `libsekejap.so`,
///    `sekejap.dll`), which the dynamic loader resolves against
///    `DYLD_LIBRARY_PATH`, `LD_LIBRARY_PATH`, the app bundle, the Android
///    `jniLibs` directory or the directory of the executable.
SekejapBindings get sekejap => _bindings ??= SekejapBindings(_load());

DynamicLibrary _load() {
  final tried = <String>[];
  Object? last;

  final explicit = _explicitPath;
  if (explicit != null) {
    tried.add(explicit);
    try {
      return DynamicLibrary.open(explicit);
    } catch (e) {
      last = e;
    }
  }

  final fromEnvironment = Platform.environment['SEKEJAP_LIBRARY'];
  if (fromEnvironment != null && fromEnvironment.isNotEmpty) {
    tried.add('SEKEJAP_LIBRARY=$fromEnvironment');
    try {
      return DynamicLibrary.open(fromEnvironment);
    } catch (e) {
      last = e;
    }
  }

  if (!Platform.isWindows) {
    tried.add('the running process');
    try {
      final process = DynamicLibrary.process();
      if (process.providesSymbol('sekejap_version')) return process;
    } catch (e) {
      last = e;
    }
  }

  for (final name in _platformNames) {
    tried.add(name);
    try {
      return DynamicLibrary.open(name);
    } catch (e) {
      last = e;
    }
  }

  throw SekejapLibraryNotFound(tried, last ?? 'no candidate was reachable');
}

List<String> get _platformNames {
  if (Platform.isMacOS || Platform.isIOS) {
    return const [
      'libsekejap.dylib',
      'sekejap.framework/sekejap',
    ];
  }
  if (Platform.isWindows) return const ['sekejap.dll', 'libsekejap.dll'];
  return const ['libsekejap.so'];
}
