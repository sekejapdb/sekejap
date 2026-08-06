import 'package:isar/isar.dart';
import '../workload.dart';
import '../bench.dart' show dirSize;
import '../models_isar.dart';

class IsarBench implements EngineBench {
  @override
  final name = 'isar';
  Isar? _db;

  @override
  Future<void> open(String dir) async {
    _db = await Isar.open([IsarDocSchema], directory: dir);
  }

  @override
  Future<void> insertAll(List<DocData> docs) async {
    final rows = [
      for (final d in docs)
        IsarDoc()
          ..key = d.id
          ..name = d.name
          ..category = d.category
          ..value = d.value
          ..ts = d.ts
    ];
    await _db!.writeTxn(() => _db!.isarDocs.putAll(rows));
  }

  @override
  Future<String?> getById(String id) async =>
      (await _db!.isarDocs.filter().keyEqualTo(id).findFirst())?.name;

  @override
  Future<int> queryCategory(String cat) =>
      _db!.isarDocs.filter().categoryEqualTo(cat).count();

  @override
  Future<int> queryRange(double lo, double hi) =>
      _db!.isarDocs.filter().valueBetween(lo, hi).count();

  @override
  Future<void> update(String id, double v) async {
    await _db!.writeTxn(() async {
      final doc = await _db!.isarDocs.filter().keyEqualTo(id).findFirst();
      if (doc != null) {
        doc.value = v;
        await _db!.isarDocs.put(doc);
      }
    });
  }

  @override
  Future<void> deleteById(String id) async {
    await _db!.writeTxn(
        () => _db!.isarDocs.filter().keyEqualTo(id).deleteFirst());
  }

  @override
  Future<void> close() async { await _db?.close(); _db = null; }

  @override
  Future<int> dbSizeBytes(String dir) async => dirSize(dir);
}
