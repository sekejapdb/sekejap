// A small but real sekejap CRUD app: add notes, list them, delete them — all
// backed by the embedded database. (Uses a temp directory for simplicity; a real
// app would use path_provider's getApplicationDocumentsDirectory().)
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:sekejap/sekejap.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await initSekejap(); // load the native library once
  runApp(const NotesApp());
}

class NotesApp extends StatelessWidget {
  const NotesApp({super.key});

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'sekejap notes',
        theme: ThemeData(colorSchemeSeed: Colors.teal, useMaterial3: true),
        home: const NotesPage(),
      );
}

class NotesPage extends StatefulWidget {
  const NotesPage({super.key});
  @override
  State<NotesPage> createState() => _NotesPageState();
}

class _NotesPageState extends State<NotesPage> {
  SekejapDb? _db;
  final _controller = TextEditingController();
  List<Map<String, dynamic>> _notes = [];

  @override
  void initState() {
    super.initState();
    _open();
  }

  Future<void> _open() async {
    final dir = Directory.systemTemp.createTempSync('sekejap_notes');
    final db = await dbOpen(path: dir.path);
    await dbExecute(
      db: db,
      sql: 'CREATE TABLE note (_key TEXT PRIMARY KEY, title TEXT)',
    );
    _db = db;
    await _reload();
  }

  Future<void> _reload() async {
    final json = await dbQuery(db: _db!, sql: 'SELECT * FROM note');
    final rows = (jsonDecode(json) as List).cast<Map<String, dynamic>>();
    setState(() {
      _notes = rows.map((r) {
        final payload = (r['payload'] as Map).cast<String, dynamic>();
        return {'key': payload['_key'], 'title': payload['title']};
      }).toList();
    });
  }

  Future<void> _add() async {
    final title = _controller.text.trim();
    if (title.isEmpty || _db == null) return;
    final key = DateTime.now().microsecondsSinceEpoch.toString();
    // Parameterised INSERT — injection-safe.
    await dbExecuteParams(
      db: _db!,
      sql: r'INSERT INTO note (_key, title) VALUES ($1, $2)',
      paramsJson: jsonEncode([key, title]),
    );
    _controller.clear();
    await _reload();
  }

  Future<void> _delete(String key) async {
    await dbExecuteParams(
      db: _db!,
      sql: r'DELETE FROM note WHERE _key = $1',
      paramsJson: jsonEncode([key]),
    );
    await _reload();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('sekejap notes')),
      body: _db == null
          ? const Center(child: CircularProgressIndicator())
          : Column(
              children: [
                Padding(
                  padding: const EdgeInsets.all(12),
                  child: Row(
                    children: [
                      Expanded(
                        child: TextField(
                          controller: _controller,
                          decoration: const InputDecoration(
                            labelText: 'New note',
                            border: OutlineInputBorder(),
                          ),
                          onSubmitted: (_) => _add(),
                        ),
                      ),
                      const SizedBox(width: 8),
                      FilledButton(onPressed: _add, child: const Text('Add')),
                    ],
                  ),
                ),
                Expanded(
                  child: _notes.isEmpty
                      ? const Center(child: Text('No notes yet — add one above.'))
                      : ListView(
                          children: [
                            for (final n in _notes)
                              ListTile(
                                title: Text(n['title'] as String),
                                trailing: IconButton(
                                  icon: const Icon(Icons.delete_outline),
                                  onPressed: () => _delete(n['key'] as String),
                                ),
                              ),
                          ],
                        ),
                ),
              ],
            ),
    );
  }
}
