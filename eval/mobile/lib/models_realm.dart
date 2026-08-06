import 'package:realm/realm.dart';
part 'models_realm.realm.dart';

@RealmModel()
class _RealmDoc {
  @PrimaryKey()
  late String key;
  late String name;
  @Indexed()
  late String category;
  late double value;   // Realm: @Indexed unsupported on double
  late int ts;
}
