// The three things every call in this wrapper does: hand the C side a
// borrowed UTF-8 string, take ownership of a string it returned, and turn a
// sentinel into an exception carrying both error channels.
//
// Nothing here is exported.

import 'dart:convert';
import 'dart:ffi';

import 'package:ffi/ffi.dart';

import 'bindings.dart';
import 'library.dart';
import 'status.dart';

/// The status of the last call on THIS thread.
SekejapStatus statusOf(Pointer<NativeDb> db) =>
    SekejapStatus.fromCode(sekejap.lastErrorCode(db));

/// The message of the last call on this thread, taking ownership of it.
String? messageOf(Pointer<NativeDb> db) => takeString(sekejap.lastError(db));

/// Throw the failure the last call left on this thread.
///
/// [db] may be [nullptr]: the error slot is thread-local, which is what lets a
/// failed open -- with no handle to carry a message -- still report.
Never throwLast(Pointer<NativeDb> db, String call) {
  final status = statusOf(db);
  final message = messageOf(db) ?? 'the call failed with no message';
  throw SekejapException(status, message, call: call);
}

/// Take ownership of a string the library returned, freeing it once.
///
/// Never used on `sekejap_version`, which points into static program data.
String? takeString(Pointer<Utf8> owned) {
  if (owned == nullptr) return null;
  final text = owned.toDartString();
  sekejap.stringFree(owned);
  return text;
}

/// A JSON array of objects keyed by column name, as Dart rows.
List<Map<String, Object?>> decodeRows(String json) => [
      for (final row in jsonDecode(json) as List<Object?>)
        (row as Map).cast<String, Object?>()
    ];

/// A JSON object, as a Dart document.
Map<String, Object?> decodeDocument(String json) =>
    (jsonDecode(json) as Map).cast<String, Object?>();

/// The parameter list the ABI takes: a JSON ARRAY, or null for none.
///
/// An empty list is sent as null rather than as `[]`, because the ABI reads
/// both as "no parameters" and null is the cheaper crossing.
Pointer<Utf8> paramsToNative(List<Object?>? params, Allocator allocator) =>
    (params == null || params.isEmpty)
        ? nullptr
        : jsonEncode(params).toNativeUtf8(allocator: allocator);

/// A nullable borrowed string.
Pointer<Utf8> textToNative(String? text, Allocator allocator) =>
    text == null ? nullptr : text.toNativeUtf8(allocator: allocator);
