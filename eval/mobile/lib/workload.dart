/// One record, the same across every engine.
class DocData {
  final String id;
  final String name;
  final String category; // one of 10
  final double value;
  final int ts;
  DocData(this.id, this.name, this.category, this.value, this.ts);
}

List<DocData> makeDocs(int n) {
  return List.generate(n, (i) {
    return DocData(
      'k$i',
      'name-$i-${i % 97}',
      'cat${i % 10}',
      (i * 37 % 10000) / 10.0,
      1700000000 + i,
    );
  });
}

/// The phases every engine implements. The runner does the timing.
abstract class EngineBench {
  String get name;
  Future<void> open(String dir);
  Future<void> insertAll(List<DocData> docs);
  Future<String?> getById(String id);
  Future<int> queryCategory(String cat);
  Future<int> queryRange(double lo, double hi);
  Future<void> update(String id, double newValue);
  Future<void> deleteById(String id);
  Future<void> close();
  Future<int> dbSizeBytes(String dir);
}
