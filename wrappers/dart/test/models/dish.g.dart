// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'dish.dart';

// **************************************************************************
// SekejapEntityGenerator
// **************************************************************************

const dishSchema = EntitySchema(
  'dishes',
  'CREATE TABLE dishes (_key TEXT PRIMARY KEY, category TEXT, price INTEGER, openNow BOOLEAN)',
  indexSql: ['CREATE INDEX ON dishes USING btree (category)'],
);

Map<String, dynamic> _$dishToJson(Dish o) => {
      '_collection': 'dishes',
      '_key': o.id,
      'category': o.category,
      'price': o.price,
      'openNow': o.openNow,
    };

Dish _$dishFromJson(Map<String, dynamic> m) => Dish(
      id: m['_key'] as String,
      category: m['category'] as String,
      price: (m['price'] as num).toInt(),
      openNow: m['openNow'] as bool,
    );

/// Typed column references for `Dish`.
class DishColumns {
  const DishColumns();
  Col<String> get id => const Col('_key');
  Col<String> get category => const Col('category');
  Col<int> get price => const Col('price');
  Col<bool> get openNow => const Col('openNow');
}

/// Typed access to the `dishes` collection: typed writes and multi-model
/// query starters (`where`, `near`, `matchText`, `rankByText`, `rankByVector`).
class DishCollection extends Collection<Dish, DishColumns> {
  final Sekejap _store;
  DishCollection(this._store);

  @override
  Sekejap get store => _store;
  @override
  String get collectionName => 'dishes';
  @override
  DishColumns get columns => const DishColumns();
  @override
  Dish Function(Map<String, dynamic>) get fromJson => _$dishFromJson;
  @override
  Map<String, dynamic> toJson(Dish entity) => _$dishToJson(entity);
  @override
  String keyOf(Dish entity) => entity.id;
}

extension DishCollectionAccess on Sekejap {
  DishCollection get dishes => DishCollection(this);
}
