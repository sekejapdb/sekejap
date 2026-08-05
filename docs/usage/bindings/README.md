# Language bindings

Every binding links the same Rust engine (directly or through the C ABI), so
behavior and the SQL surface are identical everywhere. Pick your language:

| language | page | install from |
|---|---|---|
| Python | [python.md](python.md) | PyPI |
| Rust | [rust.md](rust.md) | crates.io |
| Node.js | [node.md](node.md) | npm |
| Dart / Flutter | [dart.md](dart.md) | pub.dev |
| Kotlin / Java | [kotlin.md](kotlin.md) | Maven Central |
| Swift | [swift.md](swift.md) | Swift Package Manager |
| Go | [go.md](go.md) | Go modules |
| C / C++ | [c.md](c.md) | build from source |

There is also a command-line tool:

```bash
cargo install sekejap-cli
sekejap ./mydb "SELECT * FROM places LIMIT 5"    # path first, then SQL
sekejap ./mydb                                    # interactive shell
```

Each page shows install and a first query. The full per-language API and build
notes live in each wrapper's own README under
[`wrappers/`](../../../wrappers/).
