// The wrapper, end to end, against a real `libsekejap`.
//
// Run it with the library on the loader's path:
//
//     SEKEJAP_LIBRARY=/path/to/libsekejap.dylib dart test
//
// or `tool/test.sh /path/to/libsekejap.dylib`, which does the same.
//
// Every test name is a sentence, and every oracle the tests compare against
// is computed here in the test process rather than read back from the engine.

import 'dart:io';

import 'package:sekejap/sekejap.dart';
import 'package:test/test.dart';

late Directory _root;

Db openFresh(String name, {bool service = false}) {
  final dir = Directory('${_root.path}/$name');
  dir.createSync(recursive: true);
  return service ? Db.openService(dir.path) : Db.open(dir.path);
}

/// The collection every test writes to, declared the same way each time.
///
/// The two indexes are not decoration: QL_CONTRACT §6 answers every Tier-1
/// predicate index-side, so a predicate on a field with no index is REFUSED
/// rather than scanned, and the declaration has to carry them.
void declareDish(Db db, {bool indexes = true}) {
  db.createCollection('dish', const [
    FieldSpec('name', FieldKind.text),
    FieldSpec('price', FieldKind.integer),
    FieldSpec('vegetarian', FieldKind.boolean),
  ]);
  if (indexes) {
    db.execute('CREATE INDEX dish_price ON dish USING btree(price)');
    db.execute('CREATE INDEX dish_name ON dish USING btree(name)');
  }
}

Map<String, Object?> dish(String key, String name, int price,
        {bool vegetarian = false}) =>
    {'_key': key, 'name': name, 'price': price, 'vegetarian': vegetarian};

void main() {
  setUpAll(() {
    _root = Directory.systemTemp.createTempSync('sekejap_dart_');
  });

  tearDownAll(() {
    if (_root.existsSync()) _root.deleteSync(recursive: true);
  });

  test('the library reports a version and the disk format it reads', () {
    expect(Db.version, matches(RegExp(r'^\d+\.\d+\.\d+$')));
    expect(Db.formatVersion, greaterThan(0));
  });

  test('opening a directory twice over gives back the rows written before', () {
    final first = openFresh('reopen');
    declareDish(first);
    first.put('dish', 'd1', dish('d1', 'nasi goreng', 45000));
    first.close();

    final second = Db.open('${_root.path}/reopen');
    expect(second.get('dish', 'd1')!['name'], 'nasi goreng');
    second.close();
  });

  test('a declared collection is in the catalog with the fields it declared',
      () {
    final db = openFresh('catalog');
    expect(
        db.createCollection('dish', const [FieldSpec('name', FieldKind.text)]),
        isTrue,
        reason: 'the first declaration creates it');
    expect(
        db.createCollection('dish', const [FieldSpec('name', FieldKind.text)]),
        isFalse,
        reason: 'the second finds it already there');

    expect(db.collections(), contains('dish'));

    final shape = db.describe('dish')!;
    expect(shape.name, 'dish');
    expect(shape.fields.map((f) => f.name), contains('name'));
    expect(db.describe('no_such_collection'), isNull,
        reason: 'no such collection is a miss, not a failure');
    db.close();
  });

  test('a document written is the document read back, with its key on it', () {
    final db = openFresh('roundtrip');
    declareDish(db);
    final written = dish('d1', 'nasi goreng', 45000);
    db.put('dish', 'd1', written);

    final read = db.get('dish', 'd1')!;
    expect(read['_key'], 'd1');
    expect(read['name'], written['name']);
    expect(read['price'], written['price']);
    expect(read['vegetarian'], written['vegetarian']);

    expect(db.exists('dish', 'd1'), isTrue);
    expect(db.exists('dish', 'nobody'), isFalse);
    expect(db.get('dish', 'nobody'), isNull, reason: 'a miss is not a failure');
    db.close();
  });

  test('a batch of documents lands under one commit and counts as its rows',
      () {
    final db = openFresh('batch');
    declareDish(db);
    final batch = {
      for (var i = 0; i < 10; i++)
        'd$i': dish('d$i', 'dish $i', 10000 + i * 1000, vegetarian: i.isEven)
    };
    expect(db.putMany('dish', batch), batch.length);
    expect(db.countRows('dish'), batch.length);
    expect(db.scanCountRows('dish'), batch.length,
        reason: 'the walk and the record agree');
    db.close();
  });

  test('a query with a parameter answers exactly the rows the oracle picks',
      () {
    final db = openFresh('parameter');
    declareDish(db);
    final rows = {
      for (var i = 0; i < 20; i++)
        'd$i': dish('d$i', 'dish $i', 10000 + i * 5000)
    };
    db.putMany('dish', rows);

    const ceiling = 60000;
    final oracle = rows.values
        .where((d) => (d['price']! as int) < ceiling)
        .map((d) => d['name'])
        .toList()
      ..sort();

    final answer = db.query(
        r'SELECT name FROM dish WHERE price < $1 ORDER BY name', [ceiling]);
    expect(answer.map((r) => r['name']).toList(), oracle);
    db.close();
  });

  test('a walk of a collection visits every row exactly once', () {
    final db = openFresh('scan');
    declareDish(db);
    final rows = {
      for (var i = 0; i < 25; i++) 'd$i': dish('d$i', 'dish $i', 1000 * i)
    };
    db.putMany('dish', rows);

    final walk = db.scan('dish', pageRows: 4);
    final seen = <String>[];
    for (final page in walk.pages()) {
      expect(page.length, lessThanOrEqualTo(4),
          reason: 'a page holds at most page_rows rows');
      for (final row in page) {
        seen.add(row['_key']! as String);
      }
    }
    walk.close();

    expect(seen.length, rows.length);
    expect(seen.toSet(), rows.keys.toSet());
    db.close();
  });

  test('a paged answer delivers the same rows as the whole answer', () {
    final db = openFresh('paged');
    declareDish(db);
    db.putMany('dish',
        {for (var i = 0; i < 12; i++) 'd$i': dish('d$i', 'dish $i', 1000 * i)});

    final whole = db.query('SELECT name FROM dish ORDER BY name');
    final paged = db.stream('SELECT name FROM dish ORDER BY name', null, 5);
    final collected = paged.rows().toList();
    paged.close();

    expect(collected.map((r) => r['name']).toList(),
        whole.map((r) => r['name']).toList());
    db.close();
  });

  test('a prepared statement rebinds without compiling a second time', () {
    final db = openFresh('prepared');
    declareDish(db);
    db.putMany('dish',
        {for (var i = 0; i < 6; i++) 'd$i': dish('d$i', 'dish $i', 10000 * i)});

    final statement = db.prepare(r'SELECT name FROM dish WHERE price = $1');
    expect(statement.rebindable, Rebindable.unbound,
        reason: 'nothing is bound yet, so there is nothing to answer');

    final first = statement.query([20000]);
    expect(first.single['name'], 'dish 2');
    expect(statement.rebindable, Rebindable.yes,
        reason: 'a read-only statement rebinds');

    final second = statement.query([40000]);
    expect(second.single['name'], 'dish 4');
    statement.close();
    db.close();
  });

  test('a linked pair is one anothers neighbour in the direction it points',
      () {
    final db = openFresh('graph');
    declareDish(db);
    db.createCollection('place', const [FieldSpec('name', FieldKind.text)]);
    db.put('place', 'warung', {'_key': 'warung', 'name': 'Warung Jawa'});
    db.put('dish', 'd1', dish('d1', 'nasi goreng', 45000));
    db.put('dish', 'd2', dish('d2', 'gado gado', 38000, vegetarian: true));

    db.link('place', 'warung', 'serves', 'dish', 'd1');
    db.link('place', 'warung', 'serves', 'dish', 'd2',
        properties: {'since': 1998});

    final out = db.neighbours('place', 'warung', edgeType: 'serves');
    expect(out.map((n) => n.key).toSet(), {'d1', 'd2'});
    expect(out.every((n) => n.collection == 'dish'), isTrue);
    expect(
        out.firstWhere((n) => n.key == 'd1').document['name'], 'nasi goreng');

    final back = db.neighbours('dish', 'd1',
        edgeType: 'serves', direction: SekejapDirection.incoming);
    expect(back.single.key, 'warung');

    expect(db.scanCountEdges(), 2);
    expect(db.unlink('place', 'warung', 'serves', 'dish', 'd2'), isTrue);
    expect(db.unlink('place', 'warung', 'serves', 'dish', 'd2'), isFalse,
        reason: 'the second removal finds no edge');
    expect(db.scanCountEdges(), 1);
    db.close();
  });

  test('an edge to a row that is not there is refused as an unknown row', () {
    final db = openFresh('dangling');
    declareDish(db);
    db.put('dish', 'd1', dish('d1', 'nasi goreng', 45000));
    try {
      db.link('dish', 'd1', 'follows', 'dish', 'ghost');
      fail('a missing endpoint must not link');
    } on SekejapException catch (e) {
      expect(e.status, SekejapStatus.unknownRow);
      expect(e.message, isNotEmpty);
    }
    db.close();
  });

  test(
      'a committed transaction keeps its writes and a rolled back one keeps none',
      () {
    final db = openFresh('transaction');
    declareDish(db);

    final committed = db.transaction();
    committed.put('dish', 'k1', dish('k1', 'kept one', 1000));
    committed.put('dish', 'k2', dish('k2', 'kept two', 2000));
    committed.commit();
    expect(committed.isFinished, isTrue);
    expect(db.countRows('dish'), 2);

    final abandoned = db.transaction();
    abandoned.put('dish', 'x1', dish('x1', 'gone one', 3000));
    abandoned.put('dish', 'x2', dish('x2', 'gone two', 4000));
    abandoned.rollback();
    expect(db.countRows('dish'), 2, reason: 'the rolled back batch left none');
    expect(db.get('dish', 'x1'), isNull);

    final removing = db.transaction();
    expect(removing.delete('dish', 'k1'), isTrue);
    expect(
        removing.execute(
            r'INSERT INTO dish (_key, name, price) VALUES ($1, $2, $3)',
            ['k3', 'added in a transaction', 5000]),
        1);
    removing.commit();
    expect(db.get('dish', 'k1'), isNull);
    expect(db.get('dish', 'k3')!['name'], 'added in a transaction');

    expect(() => removing.put('dish', 'nope', dish('nope', 'nope', 0)),
        throwsStateError,
        reason: 'the handle is gone after a commit');
    db.close();
  });

  test('a delete takes the row and the edges that touch it', () {
    final db = openFresh('delete');
    declareDish(db);
    db.put('dish', 'd1', dish('d1', 'one', 1));
    db.put('dish', 'd2', dish('d2', 'two', 2));
    db.link('dish', 'd1', 'pairs', 'dish', 'd2');
    expect(db.scanCountEdges(), 1);

    expect(db.delete('dish', 'd2'), isTrue);
    expect(db.delete('dish', 'd2'), isFalse, reason: 'it is gone now');
    expect(db.scanCountEdges(), 0, reason: 'the edge went with the row');
    db.close();
  });

  test('a statement with no atomic underneath is refused by name, not emulated',
      () {
    final db = openFresh('refusal');
    declareDish(db);
    try {
      // JOIN has no atomic behind it and is refused by name (README,
      // "SQL"); SHOW TABLES, which this used to be, answers since 0.17.
      db.query('SELECT name FROM dish JOIN dish AS other ON dish._key = other._key');
      fail('a Tier-2/Tier-3 construct must not answer');
    } on SekejapException catch (e) {
      expect(e.message, isNotEmpty,
          reason: 'the refusal carries a reason, never an empty answer');
      expect(Db.lastErrorCode, isNot(SekejapStatus.ok),
          reason: 'the code is still readable on this thread');
    }
    db.close();
  });

  test('a syntax error surfaces the message the engine wrote', () {
    final db = openFresh('syntax');
    try {
      db.query('SELECT FROM WHERE');
      fail('a syntax error must not answer');
    } on SekejapException catch (e) {
      expect(e.status, SekejapStatus.invalid);
      expect(e.message, isNotEmpty);
      expect(e.call, 'query');
    }
    db.close();
  });

  test('a success clears both halves of the error slot', () {
    final db = openFresh('clears');
    declareDish(db);
    expect(
        () => db.query('SELECT FROM WHERE'), throwsA(isA<SekejapException>()));
    db.put('dish', 'd1', dish('d1', 'nasi goreng', 45000));
    expect(Db.lastErrorCode, SekejapStatus.ok);
    expect(Db.lastError, isNull);
    db.close();
  });

  test('the four calls with no atomic keep their name and refuse with a reason',
      () {
    final db = openFresh('no_atomic');

    for (final call in <(String, void Function())>[
      ('open_memory', Db.openMemory),
      ('trim_memory', db.trimMemory),
      ('compact', db.compact),
      ('show', () => db.show('SHOW TABLES')),
    ]) {
      try {
        call.$2();
        fail('${call.$1} must refuse');
      } on SekejapException catch (e) {
        expect(e.status, SekejapStatus.refused,
            reason: '${call.$1} is refused by name');
        expect(e.message, isNotEmpty);
      }
    }
    db.close();
  });

  test('the bytes on disk grow with the rows written', () {
    final db = openFresh('storage');
    declareDish(db);
    final before = db.storage();
    db.putMany('dish', {
      for (var i = 0; i < 200; i++)
        'd$i': dish('d$i', 'dish number $i', 1000 * i)
    });
    final after = db.storage();
    expect(after.totalBytes, greaterThan(before.totalBytes));
    expect(after.totalBytes, after.dataBytes + after.walBytes);

    db.checkpoint(); // true folded, false deferred: both are successes
    db.publish();
    db.close();
  });

  test('a plan is explained as text a reader can hold', () {
    final db = openFresh('explain');
    declareDish(db);
    db.put('dish', 'd1', dish('d1', 'nasi goreng', 45000));
    final plan = db.explain(r'SELECT name FROM dish WHERE price < $1', [50000]);
    expect(plan, isNotEmpty);
    db.close();
  });

  test('a service handle carries the change feed a single handle refuses', () {
    final single = openFresh('single_mode');
    try {
      single.subscribe();
      fail('single mode has no change feed');
    } on SekejapException catch (e) {
      expect(e.status, SekejapStatus.refused);
      expect(e.message, isNotEmpty);
    }
    single.close();

    final service = openFresh('service_mode', service: true);
    declareDish(service);
    final subscription = service.subscribe();
    expect(subscription, greaterThanOrEqualTo(0));

    service.put('dish', 'd1', dish('d1', 'nasi goreng', 45000));
    service.publish();

    final event = service.nextChange(subscription, timeoutMs: 2000);
    expect(event, isNotNull, reason: 'the commit raised an event');
    expect(event!.rowsAffected + event.keysTotal, greaterThan(0));
    expect(event.collectionIds, isNotEmpty,
        reason: 'the feed names the collections it touched, by numeric id');

    service.statementTimeoutMs(1000);
    service.statementTimeoutMs(0); // clears it
    service.cancel();
    expect(service.clearInterrupt(), isTrue, reason: 'a cancel was standing');
    expect(service.clearInterrupt(), isFalse, reason: 'none is now');

    expect(service.unsubscribe(subscription), isTrue);
    expect(service.unsubscribe(subscription), isFalse);
    service.close();
  });

  test('a closed database refuses every further call rather than dangling', () {
    final db = openFresh('closed');
    declareDish(db);
    final walk = db.scan('dish');
    final statement = db.prepare('SELECT * FROM dish');
    db.close();

    expect(walk.isClosed, isTrue,
        reason: 'close frees the derived handles first');
    expect(statement.isClosed, isTrue);
    expect(() => db.query('SELECT 1'), throwsStateError);
    expect(() => walk.nextPage(), throwsStateError);
    db.close(); // twice is harmless
  });
}
