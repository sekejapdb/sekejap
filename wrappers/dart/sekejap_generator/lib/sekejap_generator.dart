/// build_runner entry point for the sekejap model generator.
library;

import 'package:build/build.dart';
import 'package:source_gen/source_gen.dart';

import 'src/entity_generator.dart';

/// Emits a combined `.g.dart` part for every `@SekejapEntity` class.
Builder sekejapBuilder(BuilderOptions options) =>
    SharedPartBuilder([SekejapEntityGenerator()], 'sekejap');
