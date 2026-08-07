// Multi-model in ONE typed query: scalar + spatial + text + vector, lowered to
// a single SGQL statement and run on the real native library.
//   cargo build -p sekejap_ffi
//   flutter test test/multimodel_test.dart
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:sekejap/sekejap.dart';

import 'models/place.dart';

String _dylibPath() {
  final base = '${Directory.current.path}/../../target/debug';
  if (Platform.isMacOS) return '$base/libsekejap_ffi.dylib';
  if (Platform.isLinux) return '$base/libsekejap_ffi.so';
  throw UnsupportedError('unsupported test platform');
}

// Seed via raw INSERT so geo/vector/text route to their indexes. (Typed writes
// of vector/geo through the INSERT path are a later refinement.)
Future<void> _seed(Sekejap db) async {
  final rows = <String>[
    // id, category, lon, lat, embedding, description
    "('p1','cafe','{\"type\":\"Point\",\"coordinates\":[144.9631,-37.8102]}',[0.9,0.1,0.0],'great grilled coffee brunch')",
    "('p2','cafe','{\"type\":\"Point\",\"coordinates\":[144.9694,-37.8180]}',[0.2,0.9,0.1],'quiet coffee and cake')",
    "('p3','bar','{\"type\":\"Point\",\"coordinates\":[144.9800,-37.8200]}',[0.1,0.1,0.9],'late night cocktails')",
    // far away (Bali) — outside the 5km radius
    "('p4','cafe','{\"type\":\"Point\",\"coordinates\":[115.0870,-8.8290]}',[0.9,0.1,0.0],'grilled coffee by the beach')",
  ];
  for (final r in rows) {
    await dbExecute(
      db: db.raw,
      sql: 'INSERT INTO places '
          '(_key, category, location, embedding, description) VALUES $r',
    );
  }
}

void main() {
  test('spatial + text + vector in one typed query', () async {
    final db = await Sekejap.openInMemory(
        schema: [placeSchema], libraryPath: _dylibPath());
    await _seed(db);

    // Within 5km of Melbourne Central, mentioning "coffee", ranked by vector
    // similarity to a taste embedding.
    final results = await db.places
        .near((p) => p.location, const GeoPoint(144.9631, -37.8102),
            metres: 5000)
        .matchText((p) => p.description, 'coffee')
        .rankByVector((p) => p.embedding, const [0.9, 0.1, 0.0])
        .limitTo(10)
        .find();

    final ids = results.map((p) => p.id).toList();
    // p4 excluded (far away); p3 excluded (no "coffee"). p1 nearest in vector
    // space to [0.9,0.1,0.0] so it ranks first.
    expect(ids, isNot(contains('p4')));
    expect(ids, isNot(contains('p3')));
    expect(ids, containsAll(['p1', 'p2']));
    expect(ids.first, 'p1');

    // Entity reconstructed from the read payload (geo + scalars). Vectors live
    // in the ANN index and are not echoed on read.
    final top = results.first;
    expect(top.location.lon, closeTo(144.9631, 1e-6));
    expect(top.category, 'cafe');
  });

  test('hybrid ranking: BM25 + vector blended', () async {
    final db = await Sekejap.openInMemory(
        schema: [placeSchema], libraryPath: _dylibPath());
    await _seed(db);

    final results = await db.places
        .matchText((p) => p.description, 'coffee')
        .rankByText((p) => p.description, 'grilled', weight: 0.5)
        .rankByVector((p) => p.embedding, const [0.9, 0.1, 0.0], weight: 0.5)
        .find();

    // p1 matches "grilled" textually AND is closest in vector space → first.
    expect(results.first.id, 'p1');
  });

  test('scalar where still composes with spatial', () async {
    final db = await Sekejap.openInMemory(
        schema: [placeSchema], libraryPath: _dylibPath());
    await _seed(db);

    final n = await db.places
        .where((p) => p.category.eq('cafe'))
        .near((p) => p.location, const GeoPoint(144.9631, -37.8102),
            metres: 5000)
        .count();
    expect(n, 2); // p1, p2 (p4 cafe but far; p3 bar)
  });
}

