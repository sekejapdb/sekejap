/// Reactive change feed for sekejap.
///
/// [watchChanges] turns the native change feed into a single, cancellable
/// `Stream<ChangeEvent>`: one event per committed mutation (a transaction emits
/// once, at COMMIT). Cancelling the subscription releases the native listener.
///
/// This is the primitive a reactive query builds on — re-run a query whenever an
/// event touches its collection, keys, or edges.
library;

import 'dart:async';

import 'rust/api/simple.dart';

/// Watch the database change feed. Each committed mutation yields one
/// [ChangeEvent] naming the collections, node keys, and edge types it touched.
///
/// ```dart
/// final sub = watchChanges(db).listen((e) {
///   if (e.collections.contains('dishes')) refreshDishes();
/// });
/// // later:
/// await sub.cancel(); // releases the native listener
/// ```
///
/// The returned stream is single-subscription. Wrap it with `.asBroadcastStream()`
/// if several listeners need the same feed.
Stream<ChangeEvent> watchChanges(SekejapDb db) {
  SekejapWatch? handle;
  StreamSubscription<ChangeEvent>? inner;
  late StreamController<ChangeEvent> controller;

  Future<void> stop() async {
    final h = handle;
    handle = null;
    if (h != null) {
      // Wake the native stream loop and drop the engine listener.
      await dbWatchClose(db: db, watch: h);
    }
    await inner?.cancel();
    inner = null;
  }

  controller = StreamController<ChangeEvent>(
    onListen: () async {
      final h = await dbWatchOpen(db: db);
      handle = h;
      inner = dbWatchStream(watch: h).listen(
        controller.add,
        onError: controller.addError,
        onDone: controller.close,
      );
    },
    onCancel: stop,
  );

  return controller.stream;
}
