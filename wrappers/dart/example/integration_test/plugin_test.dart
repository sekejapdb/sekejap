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
}
