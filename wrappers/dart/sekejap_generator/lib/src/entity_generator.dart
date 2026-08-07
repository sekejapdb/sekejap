import 'package:analyzer/dart/element/element.dart';
import 'package:build/build.dart';
import 'package:source_gen/source_gen.dart';

/// Generates a typed collection, column references, and (de)serialisers for
/// every class annotated `@SekejapEntity`. Emitted as a combined `.g.dart`
/// part — it lowers to the same SGQL the engine runs, not a second engine.
class SekejapEntityGenerator extends Generator {
  static const _ns = 'package:sekejap/src/annotations.dart';
  static const _entity = TypeChecker.fromUrl('$_ns#SekejapEntity');
  static const _key = TypeChecker.fromUrl('$_ns#Key');
  static const _index = TypeChecker.fromUrl('$_ns#Index');
  static const _geo = TypeChecker.fromUrl('$_ns#Geo');
  static const _vector = TypeChecker.fromUrl('$_ns#Vector');
  static const _bm25 = TypeChecker.fromUrl('$_ns#Bm25');

  @override
  String generate(LibraryReader library, BuildStep buildStep) {
    final out = StringBuffer();
    for (final element in library.classes) {
      final ann = _entity.firstAnnotationOf(element);
      if (ann == null) continue;
      out.writeln(_forClass(element, ConstantReader(ann)));
    }
    return out.toString();
  }

  String _forClass(ClassElement cls, ConstantReader ann) {
    final className = cls.name!;
    final lower = className[0].toLowerCase() + className.substring(1);
    final collectionArg = ann.read('collection');
    final collection = collectionArg.isNull
        ? _pluralize(className.toLowerCase())
        : collectionArg.stringValue;

    final fields = <_Field>[];
    for (final f in cls.fields) {
      if (f.isStatic || f.isSynthetic) continue;
      final name = f.name!;
      final isKey = _key.hasAnnotationOfExact(f);
      final kind = _kind(f);
      // Scalar index method from @Index(kind): 1 = hash, else btree.
      String? scalarIndex;
      final idxAnn = _index.firstAnnotationOfExact(f);
      if (idxAnn != null) {
        final ordinal = ConstantReader(idxAnn).read('kind').read('index').intValue;
        scalarIndex = ordinal == 1 ? 'hash' : 'btree';
      }
      fields.add(_Field(
        dartName: name,
        column: isKey ? '_key' : name,
        dartType: _bareType(f),
        kind: kind,
        isKey: isKey,
        scalarIndex: scalarIndex,
      ));
    }
    if (!fields.any((f) => f.isKey)) {
      throw InvalidGenerationSourceError(
        '@SekejapEntity `$className` needs exactly one @Key() field.',
        element: cls,
      );
    }

    // CREATE TABLE + secondary indexes.
    final createCols = fields
        .map((f) => f.isKey
            ? '${f.column} ${f.sqlType} PRIMARY KEY'
            : '${f.column} ${f.sqlType}')
        .join(', ');
    final indexes = <String>[];
    for (final f in fields) {
      final using = f.indexUsing;
      if (using != null) {
        indexes.add("'CREATE INDEX ON $collection USING $using (${f.column})'");
      }
    }
    final indexList = indexes.isEmpty ? '' : '\n  indexSql: [${indexes.join(', ')}],';

    final toJson = fields.map((f) => "      '${f.column}': ${f.toJsonExpr('o')},").join('\n');
    final fromJson = fields.map((f) => '      ${f.dartName}: ${f.fromJsonExpr()},').join('\n');
    final colGetters = fields
        .map((f) => "  Col<${f.colType}> get ${f.dartName} => const Col('${f.column}');")
        .join('\n');
    final keyName = fields.firstWhere((f) => f.isKey).dartName;

    return '''
const ${lower}Schema = EntitySchema(
  '$collection',
  'CREATE TABLE $collection ($createCols)',$indexList
);

Map<String, dynamic> _\$${lower}ToJson($className o) => {
      '_collection': '$collection',
$toJson
    };

$className _\$${lower}FromJson(Map<String, dynamic> m) => $className(
$fromJson
    );

/// Typed column references for `$className`.
class ${className}Columns {
  const ${className}Columns();
$colGetters
}

/// Typed access to the `$collection` collection: typed writes and multi-model
/// query starters (`where`, `near`, `matchText`, `rankByText`, `rankByVector`).
class ${className}Collection extends Collection<$className, ${className}Columns> {
  final Sekejap _store;
  ${className}Collection(this._store);

  @override
  Sekejap get store => _store;
  @override
  String get collectionName => '$collection';
  @override
  ${className}Columns get columns => const ${className}Columns();
  @override
  $className Function(Map<String, dynamic>) get fromJson => _\$${lower}FromJson;
  @override
  Map<String, dynamic> toJson($className entity) => _\$${lower}ToJson(entity);
  @override
  String keyOf($className entity) => entity.$keyName;
}

extension ${className}CollectionAccess on Sekejap {
  ${className}Collection get $collection => ${className}Collection(this);
}
''';
  }

  _Kind _kind(FieldElement f) {
    if (_geo.hasAnnotationOfExact(f)) return _Kind.geo;
    if (_vector.hasAnnotationOfExact(f)) return _Kind.vector;
    if (_bm25.hasAnnotationOfExact(f)) return _Kind.bm25;
    switch (_bareType(f)) {
      case 'String':
        return _Kind.text;
      case 'int':
        return _Kind.integer;
      case 'double':
        return _Kind.real;
      case 'bool':
        return _Kind.boolean;
      default:
        return _Kind.json;
    }
  }

  String _bareType(FieldElement f) {
    var s = f.type.getDisplayString();
    if (s.endsWith('?')) s = s.substring(0, s.length - 1);
    return s;
  }

  String _pluralize(String w) {
    if (w.endsWith('s') ||
        w.endsWith('x') ||
        w.endsWith('z') ||
        w.endsWith('ch') ||
        w.endsWith('sh')) {
      return '${w}es';
    }
    if (w.endsWith('y') && w.length > 1 && !_isVowel(w[w.length - 2])) {
      return '${w.substring(0, w.length - 1)}ies';
    }
    return '${w}s';
  }

  bool _isVowel(String c) => 'aeiou'.contains(c);
}

enum _Kind { text, integer, real, boolean, geo, vector, bm25, json }

class _Field {
  final String dartName;
  final String column;
  final String dartType;
  final _Kind kind;
  final bool isKey;
  final String? scalarIndex; // 'btree' | 'hash' | null
  _Field({
    required this.dartName,
    required this.column,
    required this.dartType,
    required this.kind,
    required this.isKey,
    required this.scalarIndex,
  });

  String get sqlType {
    switch (kind) {
      case _Kind.integer:
        return 'INTEGER';
      case _Kind.real:
        return 'REAL';
      case _Kind.boolean:
        return 'BOOLEAN';
      case _Kind.geo:
        return 'GEO';
      case _Kind.vector:
        return 'VECTOR';
      case _Kind.text:
      case _Kind.bm25:
        return 'TEXT';
      case _Kind.json:
        return 'JSON';
    }
  }

  /// The type argument for the generated `Col<…>`.
  String get colType {
    switch (kind) {
      case _Kind.geo:
        return 'GeoPoint';
      case _Kind.vector:
        return 'List<double>';
      default:
        return dartType;
    }
  }

  /// `USING <method>` for a secondary index, or null.
  String? get indexUsing {
    if (isKey) return null;
    switch (kind) {
      case _Kind.geo:
        return 'spatial';
      case _Kind.vector:
        return 'hnsw';
      case _Kind.bm25:
        return 'bm25';
      default:
        return scalarIndex; // 'btree' | 'hash' | null
    }
  }

  String toJsonExpr(String o) {
    switch (kind) {
      case _Kind.geo:
        return "{'type': 'Point', 'coordinates': [$o.$dartName.lon, $o.$dartName.lat]}";
      default:
        return '$o.$dartName';
    }
  }

  String fromJsonExpr() {
    final k = "m['$column']";
    switch (kind) {
      case _Kind.text:
      case _Kind.bm25:
        return '$k as String';
      case _Kind.integer:
        return '($k as num).toInt()';
      case _Kind.real:
        return '($k as num).toDouble()';
      case _Kind.boolean:
        return '$k as bool';
      // Vectors live in the ANN index and are not always echoed in reads;
      // default to empty when absent.
      case _Kind.vector:
        return '[for (final e in (($k as List?) ?? const [])) (e as num).toDouble()]';
      case _Kind.geo:
        return 'GeoPoint('
            '(($k as Map)[\'coordinates\'][0] as num).toDouble(), '
            '($k[\'coordinates\'][1] as num).toDouble())';
      case _Kind.json:
        return k;
    }
  }
}
