// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'doc.dart';

// **************************************************************************
// SekejapEntityGenerator
// **************************************************************************

const docSchema = EntitySchema(
  'docs',
  'CREATE TABLE docs (_key TEXT PRIMARY KEY, name TEXT, category TEXT, value REAL, ts INTEGER)',
  indexSql: [
    'CREATE INDEX ON docs USING hash (category)',
    'CREATE INDEX ON docs USING btree (value)'
  ],
);

Map<String, dynamic> _$docToJson(Doc o) => {
      '_collection': 'docs',
      '_key': o.id,
      'name': o.name,
      'category': o.category,
      'value': o.value,
      'ts': o.ts,
    };

Doc _$docFromJson(Map<String, dynamic> m) => Doc(
      id: m['_key'] as String,
      name: m['name'] as String,
      category: m['category'] as String,
      value: (m['value'] as num).toDouble(),
      ts: (m['ts'] as num).toInt(),
    );

/// Typed column references for `Doc`.
class DocColumns {
  const DocColumns();
  Col<String> get id => const Col('_key');
  Col<String> get name => const Col('name');
  Col<String> get category => const Col('category');
  Col<double> get value => const Col('value');
  Col<int> get ts => const Col('ts');
}

/// Typed access to the `docs` collection: typed writes and multi-model
/// query starters (`where`, `near`, `matchText`, `rankByText`, `rankByVector`).
class DocCollection extends Collection<Doc, DocColumns> {
  final Sekejap _store;
  DocCollection(this._store);

  @override
  Sekejap get store => _store;
  @override
  String get collectionName => 'docs';
  @override
  DocColumns get columns => const DocColumns();
  @override
  Doc Function(Map<String, dynamic>) get fromJson => _$docFromJson;
  @override
  Map<String, dynamic> toJson(Doc entity) => _$docToJson(entity);
  @override
  String keyOf(Doc entity) => entity.id;
}

extension DocCollectionAccess on Sekejap {
  DocCollection get docs => DocCollection(this);
}
