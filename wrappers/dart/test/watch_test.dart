// Host-VM test of the reactive change feed against the prebuilt native library.
// Runs on the Dart VM (no app build needed):
//   cargo build -p sekejap_ffi          # produces target/debug/libsekejap_ffi.*
//   flutter test test/watch_test.dart
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:sekejap/sekejap.dart';

String _dylibPath() {
  final base = '${Directory.current.path}/../../target/debug';
  if (Platform.isMacOS) return '$base/libsekejap_ffi.dylib';
  if (Platform.isLinux) return '$base/libsekejap_ffi.so';
  if (Platform.isWindows) return '$base/sekejap_ffi.dll';
  throw UnsupportedError('unsupported test platform');
}

void main() {
  setUpAll(() async => initSekejap(libraryPath: _dylibPath()));

  test('watch emits one ChangeEvent per committed mutation', () async {
    final dir = Directory.systemTemp.createTempSync('sekejap_watch');
    addTearDown(() => dir.deleteSync(recursive: true));

    final db = await dbOpen(path: dir.path);
    await dbExecute(
        db: db, sql: 'CREATE TABLE items (_key TEXT PRIMARY KEY, v INTEGER)');

    final events = <ChangeEvent>[];
    final sub = watchChanges(db).listen(events.add);
    await Future<void>.delayed(const Duration(milliseconds: 50));

    // Single insert → one event naming the collection and key.
    await dbExecute(db: db, sql: "INSERT INTO items (_key, v) VALUES ('a', 1)");
    // Transaction of two inserts → exactly one event at COMMIT.
    await dbExecute(db: db, sql: 'BEGIN');
    await dbExecute(db: db, sql: "INSERT INTO items (_key, v) VALUES ('b', 2)");
    await dbExecute(db: db, sql: "INSERT INTO items (_key, v) VALUES ('c', 3)");
    await dbExecute(db: db, sql: 'COMMIT');
    // An edge → one event naming the edge type.
    await dbLink(db: db, from: 'items/a', to: 'items/b', edgeType: 'rel');

    await Future<void>.delayed(const Duration(milliseconds: 100));

    expect(events.length, 3, reason: 'insert + txn(once) + link = 3 events');
    expect(events[0].collections, contains('items'));
    expect(events[0].keys.any((k) => k.contains('a')), isTrue);
    expect(events[1].keys.length, greaterThanOrEqualTo(2),
        reason: 'both txn keys in the single commit event');
    expect(events[2].edgeTypes, contains('rel'));

    // Cancellation completes promptly (no polling, no hang).
    await sub.cancel().timeout(const Duration(seconds: 3));
  });

  test('rollback emits nothing', () async {
    final dir = Directory.systemTemp.createTempSync('sekejap_watch_rb');
    addTearDown(() => dir.deleteSync(recursive: true));

    final db = await dbOpen(path: dir.path);
    await dbExecute(
        db: db, sql: 'CREATE TABLE items (_key TEXT PRIMARY KEY, v INTEGER)');

    final events = <ChangeEvent>[];
    final sub = watchChanges(db).listen(events.add);
    await Future<void>.delayed(const Duration(milliseconds: 50));

    await dbExecute(db: db, sql: 'BEGIN');
    await dbExecute(db: db, sql: "INSERT INTO items (_key, v) VALUES ('a', 1)");
    await dbExecute(db: db, sql: 'ROLLBACK');

    await Future<void>.delayed(const Duration(milliseconds: 100));

    expect(events, isEmpty, reason: 'a rolled-back transaction emits nothing');
    await sub.cancel().timeout(const Duration(seconds: 3));
  });

  test('cancel stops delivery', () async {
    final dir = Directory.systemTemp.createTempSync('sekejap_watch_cancel');
    addTearDown(() => dir.deleteSync(recursive: true));

    final db = await dbOpen(path: dir.path);
    await dbExecute(
        db: db, sql: 'CREATE TABLE items (_key TEXT PRIMARY KEY)');

    final events = <ChangeEvent>[];
    final sub = watchChanges(db).listen(events.add);
    await Future<void>.delayed(const Duration(milliseconds: 50));
    await dbExecute(db: db, sql: "INSERT INTO items (_key) VALUES ('a')");
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(events.length, 1);

    await sub.cancel().timeout(const Duration(seconds: 3));

    // Mutations after cancel must not be delivered.
    await dbExecute(db: db, sql: "INSERT INTO items (_key) VALUES ('b')");
    await Future<void>.delayed(const Duration(milliseconds: 50));
    expect(events.length, 1, reason: 'no delivery after cancel');
  });
}
