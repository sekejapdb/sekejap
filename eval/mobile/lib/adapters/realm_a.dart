import 'package:realm/realm.dart';
import '../workload.dart';
import '../bench.dart' show dirSize;
import '../models_realm.dart';

class RealmBench implements EngineBench {
  @override
  final name = 'realm';
  Realm? _db;

  @override
  Future<void> open(String dir) async {
    _db = Realm(Configuration.local([RealmDoc.schema], path: '$dir/bench.realm'));
  }

  @override
  Future<void> insertAll(List<DocData> docs) async {
    _db!.write(() {
      _db!.addAll([
        for (final d in docs) RealmDoc(d.id, d.name, d.category, d.value, d.ts)
      ]);
    });
  }

  @override
  Future<String?> getById(String id) async => _db!.find<RealmDoc>(id)?.name;

  @override
  Future<int> queryCategory(String cat) async =>
      _db!.query<RealmDoc>('category == \$0', [cat]).length;

  @override
  Future<int> queryRange(double lo, double hi) async =>
      _db!.query<RealmDoc>('value >= \$0 AND value <= \$1', [lo, hi]).length;

  @override
  Future<void> update(String id, double v) async {
    final doc = _db!.find<RealmDoc>(id);
    if (doc != null) _db!.write(() => doc.value = v);
  }

  @override
  Future<void> deleteById(String id) async {
    final doc = _db!.find<RealmDoc>(id);
    if (doc != null) _db!.write(() => _db!.delete(doc));
  }

  @override
  Future<void> close() async { _db?.close(); _db = null; }

  @override
  Future<int> dbSizeBytes(String dir) async => dirSize(dir);
}
