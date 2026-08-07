import 'dart:convert';
import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';
import 'package:sekejap/sekejap.dart' as sk;

import 'bench.dart';
import 'workload.dart';
import 'adapters/sekejap_a.dart';
import 'adapters/sqflite_a.dart';
import 'adapters/objectbox_a.dart';
import 'adapters/hive_a.dart';
import 'adapters/realm_a.dart';

void main() => runApp(const BenchApp());

class BenchApp extends StatelessWidget {
  const BenchApp({super.key});
  @override
  Widget build(BuildContext context) => const MaterialApp(
        title: 'sekejap mobile bench',
        home: BenchPage(),
      );
}

class BenchPage extends StatefulWidget {
  const BenchPage({super.key});
  @override
  State<BenchPage> createState() => _BenchPageState();
}

class _BenchPageState extends State<BenchPage> {
  final _log = StringBuffer();
  bool _running = false;
  final Map<String, List<PhaseResult>> _results = {};

  void _print(String s) {
    debugPrint('[BENCH] $s');
    setState(() => _log.writeln(s));
  }

  Future<void> _run() async {
    setState(() { _running = true; _log.clear(); _results.clear(); });
    try {
      await sk.initSekejap();
      final base = (await getApplicationDocumentsDirectory()).path;
      final docs = makeDocs(10000);
      final engines = <EngineBench>[
        SekejapBench(),
        SqfliteBench(),
        ObjectBoxBench(),
        HiveBench(),
        RealmBench(),
      ];
      for (final e in engines) {
        _print('── ${e.name} ──');
        try {
          _results[e.name] = await runEngine(e, base, docs, log: _print);
        } catch (err) {
          _print('  ${e.name} FAILED: $err');
        }
      }
      // machine-readable dump for adb logcat capture
      final dump = {
        for (final en in _results.entries)
          en.key: {for (final p in en.value) p.phase: p.ms}
      };
      debugPrint('[BENCH-JSON] ${jsonEncode(dump)}');
      _print('done — JSON dumped to log');
    } finally {
      setState(() => _running = false);
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('sekejap mobile bench')),
        body: Column(children: [
          Padding(
            padding: const EdgeInsets.all(12),
            child: FilledButton(
              onPressed: _running ? null : _run,
              child: Text(_running ? 'running…' : 'RUN BENCHMARK (10k docs × 6 engines)'),
            ),
          ),
          Expanded(
            child: SingleChildScrollView(
              padding: const EdgeInsets.all(12),
              child: Text(_log.toString(),
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 11)),
            ),
          ),
        ]),
      );
}
