// End-to-end test of the sekejap plugin against the real bundled native library.
// Run on a desktop target:  flutter test integration_test -d macos   (or -d linux)
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:sekejap/sekejap.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  setUpAll(() async => initSekejap());

  test('roundtrip', () async {
    final dir = Directory.systemTemp.createTempSync('sekejap_flutter');
    addTearDown(() => dir.deleteSync(recursive: true));

    final db = await dbOpen(path: dir.path);
    await dbExecute(db: db, sql: 'CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)');
    expect(await dbExecute(db: db, sql: "INSERT INTO t (_key, v) VALUES ('a', 42)"),
        BigInt.one);

    final rows = await dbQuery(db: db, sql: "SELECT v FROM t WHERE _key = 'a'");
    expect(rows, contains('42'));

    final params = await dbQueryParams(
        db: db, sql: 'SELECT _key FROM t WHERE v = \$1', paramsJson: '[42]');
    expect(params, contains('a'));

    expect(await dbContains(db: db, slug: 't/a'), isTrue);
    await dbCompact(db: db);
  });

  test('prepared', () async {
    final dir = Directory.systemTemp.createTempSync('sekejap_flutter_prep');
    addTearDown(() => dir.deleteSync(recursive: true));

    final db = await dbOpen(path: dir.path);
    await dbExecute(db: db, sql: 'CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)');
    for (var i = 0; i < 5; i++) {
      await dbExecute(db: db, sql: "INSERT INTO t (_key, v) VALUES ('k$i', $i)");
    }

    final stmt = await dbPrepare(db: db, sql: 'SELECT _key FROM t WHERE v = \$1');
    for (var i = 0; i < 5; i++) {
      final rows = await dbQueryPrepared(db: db, stmt: stmt, paramsJson: '[$i]');
      expect(rows, contains('k$i'), reason: 'param $i: $rows');
    }
  });

  test('watch emits a ChangeEvent per committed mutation', () async {
    final dir = Directory.systemTemp.createTempSync('sekejap_flutter_watch');
    addTearDown(() => dir.deleteSync(recursive: true));

    final db = await dbOpen(path: dir.path);
    await dbExecute(
        db: db, sql: 'CREATE TABLE items (_key TEXT PRIMARY KEY, v INTEGER)');

    // Collect events off the change feed.
    final events = <ChangeEvent>[];
    final sub = watchChanges(db).listen(events.add);
    // Let the stream registration reach the Rust side before mutating.
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

    // Give the async stream a moment to drain.
    await Future<void>.delayed(const Duration(milliseconds: 100));
    await sub.cancel().timeout(const Duration(seconds: 3));

    expect(events.length, 3, reason: 'insert + txn(once) + link = 3 events');
    expect(events[0].collections, contains('items'));
    expect(events[0].keys.any((k) => k.contains('a')), isTrue);
    expect(events[1].keys.length, greaterThanOrEqualTo(2),
        reason: 'both txn keys in the single commit event');
    expect(events[2].edgeTypes, contains('rel'));
  });
}
