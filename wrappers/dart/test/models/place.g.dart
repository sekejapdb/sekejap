// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'place.dart';

// **************************************************************************
// SekejapEntityGenerator
// **************************************************************************

const placeSchema = EntitySchema(
  'places',
  'CREATE TABLE places (_key TEXT PRIMARY KEY, category TEXT, location GEO, embedding VECTOR, description TEXT)',
  indexSql: [
    'CREATE INDEX ON places USING btree (category)',
    'CREATE INDEX ON places USING spatial (location)',
    'CREATE INDEX ON places USING hnsw (embedding)',
    'CREATE INDEX ON places USING bm25 (description)'
  ],
);

Map<String, dynamic> _$placeToJson(Place o) => {
      '_collection': 'places',
      '_key': o.id,
      'category': o.category,
      'location': {
        'type': 'Point',
        'coordinates': [o.location.lon, o.location.lat]
      },
      'embedding': o.embedding,
      'description': o.description,
    };

Place _$placeFromJson(Map<String, dynamic> m) => Place(
      id: m['_key'] as String,
      category: m['category'] as String,
      location: GeoPoint(
          ((m['location'] as Map)['coordinates'][0] as num).toDouble(),
          (m['location']['coordinates'][1] as num).toDouble()),
      embedding: [
        for (final e in ((m['embedding'] as List?) ?? const []))
          (e as num).toDouble()
      ],
      description: m['description'] as String,
    );

/// Typed column references for `Place`.
class PlaceColumns {
  const PlaceColumns();
  Col<String> get id => const Col('_key');
  Col<String> get category => const Col('category');
  Col<GeoPoint> get location => const Col('location');
  Col<List<double>> get embedding => const Col('embedding');
  Col<String> get description => const Col('description');
}

/// Typed access to the `places` collection: typed writes and multi-model
/// query starters (`where`, `near`, `matchText`, `rankByText`, `rankByVector`).
class PlaceCollection extends Collection<Place, PlaceColumns> {
  final Sekejap _store;
  PlaceCollection(this._store);

  @override
  Sekejap get store => _store;
  @override
  String get collectionName => 'places';
  @override
  PlaceColumns get columns => const PlaceColumns();
  @override
  Place Function(Map<String, dynamic>) get fromJson => _$placeFromJson;
  @override
  Map<String, dynamic> toJson(Place entity) => _$placeToJson(entity);
  @override
  String keyOf(Place entity) => entity.id;
}

extension PlaceCollectionAccess on Sekejap {
  PlaceCollection get places => PlaceCollection(this);
}
