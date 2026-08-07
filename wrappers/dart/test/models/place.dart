// A multi-model entity: scalar + spatial + vector + full-text in one type.
import 'package:sekejap/sekejap.dart';

part 'place.g.dart';

@SekejapEntity()
class Place {
  @Key()
  final String id;
  @Index()
  final String category;
  @Geo()
  final GeoPoint location;
  @Vector(3)
  final List<double> embedding;
  @Bm25()
  final String description;

  const Place({
    required this.id,
    required this.category,
    required this.location,
    required this.embedding,
    required this.description,
  });
}
