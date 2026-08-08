# sekejap_generator

The `build_runner` code generator for [sekejap](https://pub.dev/packages/sekejap)'s
typed model layer. It reads `@SekejapEntity()`-annotated classes and emits the
typed collection accessor, column references, schema descriptor, and JSON
(de)serializers — the plumbing behind the typed, reactive Dart API.

This is a **dev-only** package: it runs at build time and is not shipped in your
app. It pairs with the `sekejap` runtime package (the annotations and typed base
classes), the same way `json_serializable` pairs with `json_annotation`.

## Setup

```yaml
dependencies:
  sekejap: ^0.16.2            # runtime: annotations + typed base classes
dev_dependencies:
  build_runner: ^2.4.0
  sekejap_generator: ^0.16.2  # this package — emits the .g.dart
```

## Use

Annotate a model and add the `part` directive:

```dart
import 'package:sekejap/sekejap.dart';
part 'dish.g.dart';

@SekejapEntity()
class Dish {
  @Key() final String id;
  @Index() final String category;
  final int price;
  const Dish({required this.id, required this.category, required this.price});
}
```

Then generate:

```console
dart run build_runner build
```

This produces `dish.g.dart` with `dishSchema`, `DishColumns`, `DishCollection`,
and a `db.dishes` accessor, so you can write typed, autocompleted queries:

```dart
final db = await Sekejap.open('app.db', schema: [dishSchema]);
final cheap = await db.dishes
    .where((d) => d.category.eq('main') & d.price.lt(90000))
    .sortBy((d) => d.price)
    .find();
```

See the [`sekejap` package](https://pub.dev/packages/sekejap) for the full typed
and reactive API.

## License

Dual-licensed under **MIT OR Apache-2.0** — use whichever you prefer.
