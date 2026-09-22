// One pass over the whole wrapper, printed as it goes.
//
//   dart pub get
//   SEKEJAP_LIBRARY=/path/to/libsekejap.dylib dart run example/sekejap_example.dart
//
// It writes to a temporary directory and removes it at the end.

import 'dart:io';

import 'package:sekejap/sekejap.dart';

void main() {
  final directory = Directory.systemTemp.createTempSync('sekejap_example_');
  print('libsekejap ${Db.version}, disk format ${Db.formatVersion}');
  print('database at ${directory.path}');

  final db = Db.open(directory.path);
  try {
    // ── the catalog ────────────────────────────────────────────────────────
    db.createCollection('dish', const [
      FieldSpec('name', FieldKind.text),
      FieldSpec('price', FieldKind.integer),
      FieldSpec('vegetarian', FieldKind.boolean),
    ]);
    db.createCollection('place', const [FieldSpec('name', FieldKind.text)]);
    // A Tier-1 predicate is answered index-side, so a field a WHERE names
    // needs an index; without one the query is refused rather than scanned.
    db.execute('CREATE INDEX dish_price ON dish USING btree(price)');
    print('collections: ${db.collections()}');

    // ── documents ──────────────────────────────────────────────────────────
    db.put('dish', 'nasi-goreng', {
      '_key': 'nasi-goreng',
      'name': 'Nasi Goreng',
      'price': 45000,
      'vegetarian': false,
    });
    db.putMany('dish', {
      'gado-gado': {'name': 'Gado Gado', 'price': 38000, 'vegetarian': true},
      'sate-ayam': {'name': 'Sate Ayam', 'price': 52000, 'vegetarian': false},
      'tempe-goreng': {
        'name': 'Tempe Goreng',
        'price': 18000,
        'vegetarian': true
      },
    });
    db.put('place', 'warung', {'name': 'Warung Jawa'});

    print('get:    ${db.get('dish', 'nasi-goreng')}');
    print('exists: ${db.exists('dish', 'gado-gado')}');
    print('rows:   ${db.countRows('dish')} rows in dish');

    // ── SQL with a parameter ───────────────────────────────────────────────
    final cheap =
        db.query(r'SELECT name, price FROM dish WHERE price < $1', [40000]);
    print('under 40,000: ${cheap.map((r) => r['name']).toList()}');

    // ── a prepared statement, rebound ──────────────────────────────────────
    final byCeiling = db.prepare(r'SELECT name FROM dish WHERE price < $1');
    print('rebindable before the first bind: ${byCeiling.rebindable.name}');
    for (final ceiling in [20000, 50000]) {
      final names = byCeiling.query([ceiling]).map((r) => r['name']).toList();
      print(
          'under $ceiling: $names (rebindable: ${byCeiling.rebindable.name})');
    }
    byCeiling.close();

    // ── a walk, one page at a time ─────────────────────────────────────────
    final walk = db.scan('dish', pageRows: 2);
    var page = 0;
    for (final rows in walk.pages()) {
      print('page ${page++}: ${rows.map((r) => r['_key']).toList()}');
    }
    walk.close();

    // ── edges ──────────────────────────────────────────────────────────────
    for (final key in ['nasi-goreng', 'gado-gado', 'sate-ayam']) {
      db.link('place', 'warung', 'serves', 'dish', key,
          properties: {'since': 1998});
    }
    final served = db.neighbours('place', 'warung', edgeType: 'serves');
    print('warung serves: ${served.map((n) => n.document['name']).toList()}');
    print('edges: ${db.scanCountEdges()} (by walking the keyspace)');

    // ── a transaction, committed and rolled back ───────────────────────────
    final kept = db.transaction();
    kept.put('dish', 'soto-ayam',
        {'name': 'Soto Ayam', 'price': 41000, 'vegetarian': false});
    kept.commit();

    final abandoned = db.transaction();
    abandoned.put('dish', 'never-served', {'name': 'Never Served', 'price': 1});
    abandoned.rollback();
    print('after commit + rollback: ${db.countRows('dish')} rows, '
        'never-served present: ${db.exists('dish', 'never-served')}');

    // ── the shape of a collection, and the bytes on disk ───────────────────
    final shape = db.describe('dish')!;
    print('dish: ${shape.fields.length} fields, '
        '${shape.indexes.length} indexes, rows ${shape.rows}');
    print('storage: ${db.storage()}');

    // ── an error path, with the message the engine wrote ───────────────────
    try {
      db.query('SHOW TABLES');
    } on SekejapException catch (e) {
      print('refused (${e.status.name}): ${e.message}');
    }
    try {
      db.link('dish', 'nasi-goreng', 'pairs', 'dish', 'not-a-dish');
    } on SekejapException catch (e) {
      print('refused (${e.status.name}): ${e.message}');
    }
  } finally {
    db.close();
    directory.deleteSync(recursive: true);
    print('closed');
  }
}
