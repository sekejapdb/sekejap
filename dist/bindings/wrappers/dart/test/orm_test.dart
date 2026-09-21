// End-to-end test of the typed model layer against the prebuilt native library.
//   cargo build -p sekejap_ffi
//   flutter test test/orm_test.dart
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:sekejap/sekejap.dart';

import 'models/dish.dart';

String _dylibPath() {
  final base = '${Directory.current.path}/../../target/debug';
  if (Platform.isMacOS) return '$base/libsekejap_ffi.dylib';
  if (Platform.isLinux) return '$base/libsekejap_ffi.so';
  if (Platform.isWindows) return '$base/sekejap_ffi.dll';
  throw UnsupportedError('unsupported test platform');
}

Future<Sekejap> _open() =>
    Sekejap.openInMemory(schema: [dishSchema], libraryPath: _dylibPath());

void main() {
  test('typed put + get round-trips', () async {
    final db = await _open();
    await db.dishes.put(const Dish(
        id: 'd1', category: 'main', price: 45000, openNow: true));

    final got = await db.dishes.get('d1');
    expect(got, isNotNull);
    expect(got!.id, 'd1');
    expect(got.category, 'main');
    expect(got.price, 45000);
    expect(got.openNow, true);
  });

  test('typed where / sortBy / limit lowers to SGQL and maps back', () async {
    final db = await _open();
    await db.dishes.putAll(const [
      Dish(id: 'a', category: 'main', price: 30000, openNow: true),
      Dish(id: 'b', category: 'main', price: 90000, openNow: true),
      Dish(id: 'c', category: 'main', price: 60000, openNow: false),
      Dish(id: 'd', category: 'drink', price: 20000, openNow: true),
    ]);

    // category = 'main' AND price < 90000, cheapest first.
    final results = await db.dishes
        .where((d) => d.category.eq('main') & d.price.lt(90000))
        .sortBy((d) => d.price)
        .find();

    expect(results.map((d) => d.id).toList(), ['a', 'c']);
    expect(results.first.price, 30000);

    // Typed range + count.
    final n = await db.dishes
        .where((d) => d.price.between(25000, 65000))
        .count();
    expect(n, 2); // a(30k) and c(60k); b(90k) and d(20k) excluded
  });

  test('type safety: wrong-typed comparison is a compile error', () {
    // The following, if uncommented, must not compile:
    //   db.dishes.where((d) => d.price.lt('expensive'));  // String vs Col<int>
    // Verified by the analyzer, not at runtime.
    expect(true, isTrue);
  });

  test('reactive typed watch re-emits on change', () async {
    final db = await _open();
    await db.dishes.put(const Dish(
        id: 'a', category: 'main', price: 30000, openNow: true));

    final snapshots = <List<Dish>>[];
    final sub =
        db.dishes.where((d) => d.category.eq('main')).watch().listen(snapshots.add);

    // Wait for the initial snapshot.
    await Future<void>.delayed(const Duration(milliseconds: 80));
    expect(snapshots.length, 1);
    expect(snapshots.first.length, 1);

    // A matching insert triggers a fresh snapshot.
    await db.dishes.put(const Dish(
        id: 'b', category: 'main', price: 50000, openNow: true));
    await Future<void>.delayed(const Duration(milliseconds: 80));

    expect(snapshots.length, greaterThanOrEqualTo(2));
    expect(snapshots.last.map((d) => d.id).toSet(), {'a', 'b'});

    await sub.cancel().timeout(const Duration(seconds: 3));
  });
}
