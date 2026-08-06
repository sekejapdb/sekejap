import 'package:objectbox/objectbox.dart';
import '../workload.dart';
import '../bench.dart' show dirSize;
import '../models_obx.dart';
import '../objectbox.g.dart';

class ObjectBoxBench implements EngineBench {
  @override
  final name = 'objectbox';
  Store? _store;
  Box<ObxDoc>? _box;

  @override
  Future<void> open(String dir) async {
    _store = await openStore(directory: dir);
    _box = _store!.box<ObxDoc>();
  }

  @override
  Future<void> insertAll(List<DocData> docs) async {
    _box!.putMany([
      for (final d in docs)
        ObxDoc()
          ..key = d.id
          ..name = d.name
          ..category = d.category
          ..value = d.value
          ..ts = d.ts
    ]);
  }

  @override
  Future<String?> getById(String id) async {
    final q = _box!.query(ObxDoc_.key.equals(id)).build();
    final r = q.findFirst();
    q.close();
    return r?.name;
  }

  @override
  Future<int> queryCategory(String cat) async {
    final q = _box!.query(ObxDoc_.category.equals(cat)).build();
    final n = q.count();
    q.close();
    return n;
  }

  @override
  Future<int> queryRange(double lo, double hi) async {
    final q = _box!.query(ObxDoc_.value.between(lo, hi)).build();
    final n = q.count();
    q.close();
    return n;
  }

  @override
  Future<void> update(String id, double v) async {
    final q = _box!.query(ObxDoc_.key.equals(id)).build();
    final r = q.findFirst();
    q.close();
    if (r != null) {
      r.value = v;
      _box!.put(r);
    }
  }

  @override
  Future<void> deleteById(String id) async {
    final q = _box!.query(ObxDoc_.key.equals(id)).build();
    q.remove();
    q.close();
  }

  @override
  Future<void> close() async { _store?.close(); _store = null; _box = null; }

  @override
  Future<int> dbSizeBytes(String dir) async => dirSize(dir);
}
