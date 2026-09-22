/// Dart bindings for **sekejap** -- an embedded graph-first, multi-model
/// database (SQL + graph + vector + spatial).
///
/// The binding is `dart:ffi` over the C ABI of `libsekejap`
/// (`docs/dist/C_ABI.md`): one class per C handle -- [Db], [Statement],
/// [Scan], [Transaction] -- and one method per C function. Documents,
/// parameters and rows are Dart's own JSON types.
///
/// ```dart
/// import 'package:sekejap/sekejap.dart';
///
/// void main() {
///   final db = Db.open('/tmp/mydb');
///   db.createCollection('dish', const [
///     FieldSpec('name', FieldKind.text),
///     FieldSpec('price', FieldKind.integer),
///   ]);
///   db.put('dish', 'd1', {'_key': 'd1', 'name': 'nasi goreng', 'price': 45000});
///
///   final cheap = db.query(
///       'SELECT name, price FROM dish WHERE price < \$1 ORDER BY price', [90000]);
///   print(cheap); // [{name: nasi goreng, price: 45000}]
///
///   db.close();
/// }
/// ```
///
/// The wrapper builds no native code. It loads a `libsekejap` that is already
/// on the machine, in this order: the path given to [useSekejapLibrary], the
/// `SEKEJAP_LIBRARY` environment variable, the running process, then the
/// platform's plain library name. See the README for where each one comes
/// from.
library;

export 'src/db.dart' show Db;
export 'src/library.dart' show useSekejapLibrary, sekejapLibraryLoaded;
export 'src/scan.dart' show Scan, ScanPaging;
export 'src/statement.dart' show Statement;
export 'src/status.dart'
    show
        Rebindable,
        SekejapDirection,
        SekejapException,
        SekejapLibraryNotFound,
        SekejapStatus;
export 'src/tx.dart' show Transaction;
export 'src/types.dart'
    show
        ChangeEvent,
        ChangedKey,
        CollectionDescription,
        FieldInfo,
        FieldKind,
        FieldSpec,
        IndexInfo,
        IoMode,
        Neighbour,
        StorageBytes,
        StoreConfig,
        SyncMode;
