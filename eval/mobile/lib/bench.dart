import 'dart:io';
import 'workload.dart';

class PhaseResult {
  final String phase;
  final double ms;
  PhaseResult(this.phase, this.ms);
}

Future<List<PhaseResult>> runEngine(
  EngineBench e,
  String baseDir,
  List<DocData> docs, {
  void Function(String)? log,
}) async {
  final dir = '$baseDir/${e.name}';
  final d = Directory(dir);
  if (d.existsSync()) d.deleteSync(recursive: true);
  d.createSync(recursive: true);

  final out = <PhaseResult>[];
  Future<void> phase(String label, Future<void> Function() body) async {
    final sw = Stopwatch()..start();
    await body();
    sw.stop();
    out.add(PhaseResult(label, sw.elapsedMicroseconds / 1000.0));
    log?.call('  ${e.name} $label: ${(sw.elapsedMicroseconds / 1000.0).toStringAsFixed(1)} ms');
  }

  await phase('open', () => e.open(dir));
  await phase('insert_${docs.length}', () => e.insertAll(docs));
  await phase('get_1000', () async {
    for (var i = 0; i < 1000; i++) {
      final r = await e.getById('k${(i * 7) % docs.length}');
      assert(r != null);
    }
  });
  await phase('query_cat_100', () async {
    for (var i = 0; i < 100; i++) {
      final n = await e.queryCategory('cat${i % 10}');
      assert(n > 0);
    }
  });
  await phase('query_range_100', () async {
    for (var i = 0; i < 100; i++) {
      final n = await e.queryRange(100.0 + i, 600.0 + i);
      assert(n > 0);
    }
  });
  await phase('update_1000', () async {
    for (var i = 0; i < 1000; i++) {
      await e.update('k${(i * 11) % docs.length}', 9999.0 + i);
    }
  });
  await phase('delete_1000', () async {
    for (var i = 0; i < 1000; i++) {
      await e.deleteById('k${docs.length - 1 - i}');
    }
  });
  await phase('reopen', () async {
    await e.close();
    await e.open(dir);
  });
  final size = await e.dbSizeBytes(dir);
  out.add(PhaseResult('disk_kb', size / 1024.0));
  await e.close();
  return out;
}

int dirSize(String path) {
  final d = Directory(path);
  if (!d.existsSync()) return 0;
  var total = 0;
  for (final f in d.listSync(recursive: true)) {
    if (f is File) total += f.lengthSync();
  }
  return total;
}
