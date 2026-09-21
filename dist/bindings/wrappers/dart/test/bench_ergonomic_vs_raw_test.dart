// Ergonomic-overhead benchmark: the SAME workload (N=10000, identical schema
// and indexes) run two ways on the SAME native engine —
//   RAW: string SQL + JSON, as the mobile adapter does today, and
//   ERGONOMIC: the typed model layer (db.docs.put / where().count() / update()).
// The delta between them is the cost of the ergonomic layer (serialisation +
// SQL building in Dart + fuller deserialisation), isolated from disk/engine.
//
//   cargo build -p sekejap_ffi
//   flutter test test/bench_ergonomic_vs_raw_test.dart
//
// NOTE: `flutter test` runs the Dart VM in JIT; absolute times are NOT device
// numbers. The RATIO (ergonomic / raw) is the ergonomic tax and is the point.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:sekejap/sekejap.dart' as sk;
import 'package:sekejap/sekejap.dart';

import 'models/doc.dart';

const int kN = 10000;

String _dylibPath() {
  final base = '${Directory.current.path}/../../target/debug';
  if (Platform.isMacOS) return '$base/libsekejap_ffi.dylib';
  if (Platform.isLinux) return '$base/libsekejap_ffi.so';
  throw UnsupportedError('unsupported test platform');
}

List<Doc> _makeDocs(int n) => List.generate(
      n,
      (i) => Doc(
        id: 'k$i',
        name: 'name-$i-${i % 97}',
        category: 'cat${i % 10}',
        value: (i * 37 % 10000) / 10.0,
        ts: 1700000000 + i,
      ),
    );

typedef Phases = Map<String, double>;

Future<double> _time(Future<void> Function() body) async {
  final sw = Stopwatch()..start();
  await body();
  sw.stop();
  return sw.elapsedMicroseconds / 1000.0;
}

// ── RAW adapter: mirrors eval/mobile/lib/adapters/sekejap_a.dart ──────────────
Future<Phases> _runRaw(String dir, List<Doc> docs) async {
  final p = <String, double>{};
  late sk.SekejapDb db;

  p['open'] = await _time(() async {
    db = await sk.dbOpen(path: dir);
    await sk.dbMobileProfile(db: db);
    try {
      await sk.dbExecute(db: db, sql:
          'CREATE TABLE docs (_key TEXT PRIMARY KEY, name TEXT, category TEXT, value REAL, ts INTEGER)');
      await sk.dbExecute(db: db, sql: 'CREATE INDEX ON docs USING hash (category)');
      await sk.dbExecute(db: db, sql: 'CREATE INDEX ON docs USING btree (value)');
    } catch (_) {}
  });

  p['insert'] = await _time(() async {
    final pairs = [
      for (final d in docs)
        (
          'docs/${d.id}',
          jsonEncode({
            '_collection': 'docs', '_key': d.id, 'name': d.name,
            'category': d.category, 'value': d.value, 'ts': d.ts,
          })
        )
    ];
    await sk.dbPutMany(db: db, pairs: pairs);
  });

  p['get_1000'] = await _time(() async {
    for (var i = 0; i < 1000; i++) {
      final s = await sk.dbGet(db: db, slug: 'docs/k${(i * 7) % docs.length}');
      if (s != null) jsonDecode(s);
    }
  });

  p['query_cat_100'] = await _time(() async {
    for (var i = 0; i < 100; i++) {
      await sk.dbQueryParams(
          db: db,
          sql: 'SELECT COUNT(*) AS n FROM docs WHERE category = \$1',
          paramsJson: jsonEncode(['cat${i % 10}']));
    }
  });

  p['query_range_100'] = await _time(() async {
    for (var i = 0; i < 100; i++) {
      await sk.dbQueryParams(
          db: db,
          sql: 'SELECT COUNT(*) AS n FROM docs WHERE value >= \$1 AND value <= \$2',
          paramsJson: jsonEncode([100.0 + i, 600.0 + i]));
    }
  });

  p['update_1000'] = await _time(() async {
    for (var i = 0; i < 1000; i++) {
      await sk.dbExecuteParams(
          db: db,
          sql: 'UPDATE docs SET value = \$1 WHERE _key = \$2',
          paramsJson: jsonEncode([9999.0 + i, 'k${(i * 11) % docs.length}']));
    }
  });

  p['delete_1000'] = await _time(() async {
    for (var i = 0; i < 1000; i++) {
      await sk.dbExecuteParams(
          db: db,
          sql: 'DELETE FROM docs WHERE _key = \$1',
          paramsJson: jsonEncode(['k${docs.length - 1 - i}']));
    }
  });

  await sk.dbCompact(db: db);
  return p;
}

// ── ERGONOMIC adapter: the typed model layer ─────────────────────────────────
Future<Phases> _runErgo(String dir, List<Doc> docs) async {
  final p = <String, double>{};
  late Sekejap db;

  p['open'] = await _time(() async {
    db = await Sekejap.open(dir, schema: [docSchema], libraryPath: _dylibPath());
    await sk.dbMobileProfile(db: db.raw); // same durability profile as raw
  });

  p['insert'] = await _time(() => db.docs.putAll(docs));

  p['get_1000'] = await _time(() async {
    for (var i = 0; i < 1000; i++) {
      await db.docs.get('k${(i * 7) % docs.length}');
    }
  });

  p['query_cat_100'] = await _time(() async {
    for (var i = 0; i < 100; i++) {
      await db.docs.where((d) => d.category.eq('cat${i % 10}')).count();
    }
  });

  p['query_range_100'] = await _time(() async {
    for (var i = 0; i < 100; i++) {
      await db.docs
          .where((d) => d.value.gte(100.0 + i) & d.value.lte(600.0 + i))
          .count();
    }
  });

  p['update_1000'] = await _time(() async {
    for (var i = 0; i < 1000; i++) {
      await db.docs
          .where((d) => d.id.eq('k${(i * 11) % docs.length}'))
          .update({'value': 9999.0 + i});
    }
  });

  p['delete_1000'] = await _time(() async {
    for (var i = 0; i < 1000; i++) {
      await db.docs.where((d) => d.id.eq('k${docs.length - 1 - i}')).deleteAll();
    }
  });

  await db.compact();
  return p;
}

void main() {
  test('ergonomic overhead vs raw (N=$kN)', () async {
    final docs = _makeDocs(kN);
    final tmp = Directory.systemTemp.createTempSync('sk_bench');
    addTearDown(() => tmp.deleteSync(recursive: true));

    // Warm the native lib once so `open` timing excludes first-load cost.
    await initSekejap(libraryPath: _dylibPath());

    final raw = await _runRaw('${tmp.path}/raw', docs);
    final ergo = await _runErgo('${tmp.path}/ergo', docs);

    final phases = ['open', 'insert', 'get_1000', 'query_cat_100',
        'query_range_100', 'update_1000', 'delete_1000'];

    final b = StringBuffer()
      ..writeln('\n=== Ergonomic overhead vs raw (N=$kN, host-VM JIT) ===')
      ..writeln('${'phase'.padRight(16)}${'raw ms'.padLeft(12)}'
          '${'ergo ms'.padLeft(12)}${'overhead'.padLeft(12)}');
    for (final ph in phases) {
      final r = raw[ph]!, e = ergo[ph]!;
      final ratio = r == 0 ? double.nan : e / r;
      b.writeln('${ph.padRight(16)}${r.toStringAsFixed(1).padLeft(12)}'
          '${e.toStringAsFixed(1).padLeft(12)}'
          '${'${ratio.toStringAsFixed(2)}x'.padLeft(12)}');
    }
    // ignore: avoid_print
    print(b.toString());

    // Sanity: every phase completed and produced a positive time.
    for (final ph in phases) {
      expect(raw[ph], greaterThan(0));
      expect(ergo[ph], greaterThan(0));
    }
  }, timeout: const Timeout(Duration(minutes: 5)));
}
