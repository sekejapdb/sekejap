// The benchmark entity — same shape and indexes as the raw mobile adapter's
// `docs` table (hash on category, btree on value), so raw-vs-ergonomic isolates
// only the ergonomic overhead.
import 'package:sekejap/sekejap.dart';

part 'doc.g.dart';

@SekejapEntity()
class Doc {
  @Key()
  final String id;
  final String name;
  @Index(IndexKind.hash)
  final String category;
  @Index() // btree
  final double value;
  final int ts;

  const Doc({
    required this.id,
    required this.name,
    required this.category,
    required this.value,
    required this.ts,
  });
}
