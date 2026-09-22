// The two error channels of the C ABI, in Dart: the closed status code and
// the message. `docs/dist/C_ABI.md` §1.1.

/// Why the last call failed. A closed enumeration, so a caller maps a failure
/// without parsing the message text.
enum SekejapStatus {
  /// The last call succeeded, or answered a clean miss.
  ok(0),

  /// A construct sekejap has no atomic for, named with its reason: a
  /// Tier-2/Tier-3 statement, an in-memory open, a payload-rewriting compact,
  /// a service call on a single-mode handle, a read-only store.
  refused(1),

  /// A page or a log failed verification. Nothing was changed.
  corrupt(2),

  /// A format, policy or configuration this build does not implement.
  unsupported(3),

  /// The directory, the file or the medium refused.
  io(4),

  /// The caller's arguments: a null pointer, text that is not UTF-8, JSON
  /// that does not parse, a collection that is not in the catalog, a
  /// parameter of the wrong type, a syntax error.
  invalid(5),

  /// A bound refused rather than waiting: a work budget, a statement
  /// deadline, a cancel, a second writer, a reader slot.
  busy(6),

  /// The named row is not in the collection, on a call that needs it to
  /// exist -- an edge endpoint.
  unknownRow(7),

  /// Nothing above classified it, including a panic caught at the boundary.
  /// The message is still there.
  unknown(8);

  const SekejapStatus(this.code);

  /// The `int32_t` this status is on the wire.
  final int code;

  /// The status for a code the library returned. An unrecognised code is
  /// [SekejapStatus.unknown] rather than a thrown range error: the ABI is
  /// allowed to grow its enumeration additively.
  static SekejapStatus fromCode(int code) {
    for (final status in SekejapStatus.values) {
      if (status.code == code) return status;
    }
    return SekejapStatus.unknown;
  }
}

/// A call on `libsekejap` that failed, carrying both error channels: the
/// [status] a caller switches on and the [message] `sekejap_last_error` wrote
/// on this thread.
class SekejapException implements Exception {
  SekejapException(this.status, this.message, {this.call});

  /// The closed code for the failure.
  final SekejapStatus status;

  /// The sentence the engine wrote. A refusal names the construct and the
  /// reason it has no atomic underneath.
  final String message;

  /// The wrapper call that failed, where the wrapper knows it.
  final String? call;

  /// Whether this failure is a refusal: a construct sekejap has no atomic for.
  bool get isRefusal => status == SekejapStatus.refused;

  @override
  String toString() => call == null
      ? 'SekejapException(${status.name}): $message'
      : 'SekejapException(${status.name}) in $call: $message';
}

/// `libsekejap` could not be found or could not be loaded.
class SekejapLibraryNotFound implements Exception {
  SekejapLibraryNotFound(this.tried, this.cause);

  /// The paths and names that were tried, in order.
  final List<String> tried;

  /// What the last attempt threw.
  final Object cause;

  @override
  String toString() => 'SekejapLibraryNotFound: could not load libsekejap. '
      'Tried: ${tried.join(", ")}. Last failure: $cause. '
      'Set SEKEJAP_LIBRARY to the file, or call '
      'useSekejapLibrary(path) before the first database call. '
      'The library is the `sekejap-capi` crate (dist/ffi); release builds are '
      'attached to each GitHub release as libsekejap-<platform>.tar.gz.';
}

/// Which way an edge points, for `sekejap_neighbours`.
enum SekejapDirection {
  /// Edges that leave the row.
  outgoing(0),

  /// Edges that arrive at the row.
  incoming(1),

  /// Both, with each neighbour reported once.
  both(2);

  const SekejapDirection(this.code);

  /// The `int32_t` this direction is on the wire.
  final int code;
}

/// The answer of `Statement.rebindable`: whether a further bind of a prepared
/// statement compiles nothing.
enum Rebindable {
  /// A further bind compiles nothing.
  yes,

  /// A further bind compiles. Every WRITING statement answers this, because a
  /// write folds its document at compile.
  no,

  /// The statement has not been bound yet, so there is nothing to answer.
  /// `SEKEJAP_REBIND_UNBOUND`. Not a failure.
  unbound,
}
