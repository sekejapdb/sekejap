import 'package:isar/isar.dart';
part 'models_isar.g.dart';

@collection
class IsarDoc {
  Id isarId = Isar.autoIncrement;
  @Index(unique: true)
  late String key;
  late String name;
  @Index()
  late String category;
  @Index()
  late double value;
  late int ts;
}
