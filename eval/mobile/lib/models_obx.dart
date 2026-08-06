import 'package:objectbox/objectbox.dart';

@Entity()
class ObxDoc {
  @Id()
  int id = 0;
  @Unique()
  late String key;
  late String name;
  @Index()
  late String category;
  late double value;   // ObjectBox: @Index unsupported on double — range scans
  late int ts;
}
