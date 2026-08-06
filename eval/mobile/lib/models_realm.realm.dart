// dart format width=80
// GENERATED CODE - DO NOT MODIFY BY HAND

part of 'models_realm.dart';

// **************************************************************************
// RealmObjectGenerator
// **************************************************************************

// coverage:ignore-file
// ignore_for_file: type=lint
class RealmDoc extends _RealmDoc
    with RealmEntity, RealmObjectBase, RealmObject {
  RealmDoc(
    String key,
    String name,
    String category,
    double value,
    int ts,
  ) {
    RealmObjectBase.set(this, 'key', key);
    RealmObjectBase.set(this, 'name', name);
    RealmObjectBase.set(this, 'category', category);
    RealmObjectBase.set(this, 'value', value);
    RealmObjectBase.set(this, 'ts', ts);
  }

  RealmDoc._();

  @override
  String get key => RealmObjectBase.get<String>(this, 'key') as String;
  @override
  set key(String value) => RealmObjectBase.set(this, 'key', value);

  @override
  String get name => RealmObjectBase.get<String>(this, 'name') as String;
  @override
  set name(String value) => RealmObjectBase.set(this, 'name', value);

  @override
  String get category =>
      RealmObjectBase.get<String>(this, 'category') as String;
  @override
  set category(String value) => RealmObjectBase.set(this, 'category', value);

  @override
  double get value => RealmObjectBase.get<double>(this, 'value') as double;
  @override
  set value(double value) => RealmObjectBase.set(this, 'value', value);

  @override
  int get ts => RealmObjectBase.get<int>(this, 'ts') as int;
  @override
  set ts(int value) => RealmObjectBase.set(this, 'ts', value);

  @override
  Stream<RealmObjectChanges<RealmDoc>> get changes =>
      RealmObjectBase.getChanges<RealmDoc>(this);

  @override
  Stream<RealmObjectChanges<RealmDoc>> changesFor([List<String>? keyPaths]) =>
      RealmObjectBase.getChangesFor<RealmDoc>(this, keyPaths);

  @override
  RealmDoc freeze() => RealmObjectBase.freezeObject<RealmDoc>(this);

  EJsonValue toEJson() {
    return <String, dynamic>{
      'key': key.toEJson(),
      'name': name.toEJson(),
      'category': category.toEJson(),
      'value': value.toEJson(),
      'ts': ts.toEJson(),
    };
  }

  static EJsonValue _toEJson(RealmDoc value) => value.toEJson();
  static RealmDoc _fromEJson(EJsonValue ejson) {
    if (ejson is! Map<String, dynamic>) return raiseInvalidEJson(ejson);
    return switch (ejson) {
      {
        'key': EJsonValue key,
        'name': EJsonValue name,
        'category': EJsonValue category,
        'value': EJsonValue value,
        'ts': EJsonValue ts,
      } =>
        RealmDoc(
          fromEJson(key),
          fromEJson(name),
          fromEJson(category),
          fromEJson(value),
          fromEJson(ts),
        ),
      _ => raiseInvalidEJson(ejson),
    };
  }

  static final schema = () {
    RealmObjectBase.registerFactory(RealmDoc._);
    register(_toEJson, _fromEJson);
    return const SchemaObject(ObjectType.realmObject, RealmDoc, 'RealmDoc', [
      SchemaProperty('key', RealmPropertyType.string, primaryKey: true),
      SchemaProperty('name', RealmPropertyType.string),
      SchemaProperty('category', RealmPropertyType.string,
          indexType: RealmIndexType.regular),
      SchemaProperty('value', RealmPropertyType.double),
      SchemaProperty('ts', RealmPropertyType.int),
    ]);
  }();

  @override
  SchemaObject get objectSchema => RealmObjectBase.getSchema(this) ?? schema;
}
