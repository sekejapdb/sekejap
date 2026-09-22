package life.sekejap;

import java.io.File;
import java.io.InputStream;
import java.lang.foreign.AddressLayout;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;

/**
 * The Panama/FFM downcall layer over the sekejap C ABI
 * ({@code dist/ffi/include/sekejap.h}, contract {@code docs/dist/C_ABI.md}).
 * Package-private: the public API is {@link Db}.
 *
 * <p>Written in Java rather than Kotlin because {@code MethodHandle.invokeExact}
 * is signature-polymorphic and Kotlin cannot spell such a call site.
 *
 * <p>All 59 functions of the header have a handle here. A handle crosses this
 * boundary as a {@code long} address; a {@code char*} the library returns is
 * read once and freed with {@code sekejap_string_free}, except
 * {@code sekejap_version}, which points into static program data and is never
 * freed.
 *
 * <p>The header spells row, edge and subscription counts as C {@code long},
 * which is 64-bit under LP64 (macOS, Linux) and 32-bit under LLP64 (Windows).
 * The layout for those ten functions is chosen at class-init from the running
 * platform, so one jar is correct on both.
 */
final class Ffi {
    private Ffi() {}

    private static final Linker LINKER = Linker.nativeLinker();
    private static final SymbolLookup LOOKUP = loadLibrary();

    private static final AddressLayout PTR = ValueLayout.ADDRESS;
    private static final ValueLayout.OfInt I32 = ValueLayout.JAVA_INT;
    private static final ValueLayout.OfLong I64 = ValueLayout.JAVA_LONG;

    /** True where C {@code long} is 32 bits wide: Windows LLP64. */
    private static final boolean C_LONG_IS_32 =
        System.getProperty("os.name", "").toLowerCase().contains("win");
    /** The layout of C {@code long} on this platform. */
    private static final ValueLayout C_LONG = C_LONG_IS_32 ? I32 : I64;

    // ── locating libsekejap ──────────────────────────────────────────────────

    private static SymbolLookup loadLibrary() {
        String name = System.mapLibraryName("sekejap"); // libsekejap.dylib / .so / sekejap.dll

        // 1. An explicit path.
        String explicit = System.getProperty("sekejap.lib");
        if (explicit != null) {
            return SymbolLookup.libraryLookup(explicit, Arena.global());
        }
        // 2. java.library.path -- a development build, pointing at the directory
        //    that holds the library.
        for (String dir : System.getProperty("java.library.path", "").split(File.pathSeparator)) {
            if (dir.isEmpty()) continue;
            File f = new File(dir, name);
            if (f.exists()) {
                return SymbolLookup.libraryLookup(f.getAbsolutePath(), Arena.global());
            }
        }
        // 3. Bundled in the jar under /natives/<os>-<arch>/ -- extract, then load.
        String resource = "/natives/" + platformDir() + "/" + name;
        try (InputStream in = Ffi.class.getResourceAsStream(resource)) {
            if (in != null) {
                Path tmp = Files.createTempFile("libsekejap", suffixOf(name));
                tmp.toFile().deleteOnExit();
                Files.copy(in, tmp, StandardCopyOption.REPLACE_EXISTING);
                return SymbolLookup.libraryLookup(tmp.toAbsolutePath().toString(), Arena.global());
            }
        } catch (Exception e) {
            throw new RuntimeException("failed to extract the bundled native " + resource, e);
        }
        throw new RuntimeException(
            "libsekejap not found -- set -Dsekejap.lib=<path/to/" + name + ">, put its directory "
            + "on -Djava.library.path, or use a jar built with bundled natives (" + resource + ")");
    }

    /** The os+arch key the release workflow stages resources under, e.g. "macos-aarch64". */
    private static String platformDir() {
        String os = System.getProperty("os.name", "").toLowerCase();
        String arch = System.getProperty("os.arch", "").toLowerCase();
        String osKey = (os.contains("mac") || os.contains("darwin")) ? "macos"
                     : os.contains("win") ? "windows" : "linux";
        String archKey = (arch.equals("aarch64") || arch.equals("arm64")) ? "aarch64"
                       : (arch.equals("amd64") || arch.equals("x86_64")) ? "x86_64" : arch;
        return osKey + "-" + archKey;
    }

    private static String suffixOf(String name) {
        int dot = name.lastIndexOf('.');
        return dot >= 0 ? name.substring(dot) : ".lib";
    }

    private static MethodHandle h(String name, FunctionDescriptor d) {
        return LINKER.downcallHandle(
            LOOKUP.find(name).orElseThrow(
                () -> new RuntimeException("libsekejap is missing the symbol " + name)), d);
    }

    // ── the 59 handles, in the order of docs/dist/C_ABI.md section 4 ─────────

    // 4.1 opening and identity
    private static final MethodHandle OPEN = h("sekejap_open", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle OPEN_WITH_CONFIG = h("sekejap_open_with_config", FunctionDescriptor.of(PTR, PTR, PTR));
    private static final MethodHandle OPEN_SERVICE = h("sekejap_open_service", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle CLOSE = h("sekejap_close", FunctionDescriptor.ofVoid(PTR));
    private static final MethodHandle VERSION = h("sekejap_version", FunctionDescriptor.of(PTR));
    private static final MethodHandle FORMAT_VERSION = h("sekejap_format_version", FunctionDescriptor.of(I32));

    // 4.2 errors and memory
    private static final MethodHandle LAST_ERROR = h("sekejap_last_error", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle LAST_ERROR_CODE = h("sekejap_last_error_code", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle STRING_FREE = h("sekejap_string_free", FunctionDescriptor.ofVoid(PTR));

    // 4.3 documents
    private static final MethodHandle PUT = h("sekejap_put", FunctionDescriptor.of(I32, PTR, PTR, PTR, PTR));
    private static final MethodHandle PUT_MANY = h("sekejap_put_many", FunctionDescriptor.of(C_LONG, PTR, PTR, PTR));
    private static final MethodHandle GET = h("sekejap_get", FunctionDescriptor.of(PTR, PTR, PTR, PTR));
    private static final MethodHandle EXISTS = h("sekejap_exists", FunctionDescriptor.of(I32, PTR, PTR, PTR));
    private static final MethodHandle DELETE = h("sekejap_delete", FunctionDescriptor.of(I32, PTR, PTR, PTR));
    private static final MethodHandle SCAN_OPEN = h("sekejap_scan_open", FunctionDescriptor.of(PTR, PTR, PTR, I64));
    private static final MethodHandle SCAN_NEXT = h("sekejap_scan_next", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle SCAN_CLOSE = h("sekejap_scan_close", FunctionDescriptor.ofVoid(PTR));

    // 4.4 SQL
    private static final MethodHandle EXECUTE = h("sekejap_execute", FunctionDescriptor.of(C_LONG, PTR, PTR, PTR));
    private static final MethodHandle QUERY = h("sekejap_query", FunctionDescriptor.of(PTR, PTR, PTR, PTR));
    private static final MethodHandle EXPLAIN = h("sekejap_explain", FunctionDescriptor.of(PTR, PTR, PTR, PTR));
    private static final MethodHandle PREPARE = h("sekejap_prepare", FunctionDescriptor.of(PTR, PTR, PTR));
    private static final MethodHandle STMT_QUERY = h("sekejap_stmt_query", FunctionDescriptor.of(PTR, PTR, PTR));
    private static final MethodHandle STMT_EXECUTE = h("sekejap_stmt_execute", FunctionDescriptor.of(C_LONG, PTR, PTR));
    private static final MethodHandle STMT_REBINDABLE = h("sekejap_stmt_rebindable", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle STMT_FREE = h("sekejap_stmt_free", FunctionDescriptor.ofVoid(PTR));
    private static final MethodHandle QUERY_OPEN = h("sekejap_query_open", FunctionDescriptor.of(PTR, PTR, PTR, PTR, I64));
    private static final MethodHandle QUERY_NEXT = h("sekejap_query_next", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle QUERY_CLOSE = h("sekejap_query_close", FunctionDescriptor.ofVoid(PTR));

    // 4.5 edges
    private static final MethodHandle LINK = h("sekejap_link", FunctionDescriptor.of(I32, PTR, PTR, PTR, PTR, PTR, PTR));
    private static final MethodHandle LINK_WITH = h("sekejap_link_with", FunctionDescriptor.of(I32, PTR, PTR, PTR, PTR, PTR, PTR, PTR));
    private static final MethodHandle UNLINK = h("sekejap_unlink", FunctionDescriptor.of(I32, PTR, PTR, PTR, PTR, PTR, PTR));
    private static final MethodHandle NEIGHBOURS = h("sekejap_neighbours", FunctionDescriptor.of(PTR, PTR, PTR, PTR, PTR, I32, I64));

    // 4.6 the catalog
    private static final MethodHandle CREATE_COLLECTION = h("sekejap_create_collection", FunctionDescriptor.of(I32, PTR, PTR, PTR));
    private static final MethodHandle DROP_COLLECTION = h("sekejap_drop_collection", FunctionDescriptor.of(I32, PTR, PTR));
    private static final MethodHandle COLLECTIONS = h("sekejap_collections", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle DESCRIBE = h("sekejap_describe", FunctionDescriptor.of(PTR, PTR, PTR));
    private static final MethodHandle COUNT_ROWS = h("sekejap_count_rows", FunctionDescriptor.of(C_LONG, PTR, PTR));
    private static final MethodHandle SCAN_COUNT_ROWS = h("sekejap_scan_count_rows", FunctionDescriptor.of(C_LONG, PTR, PTR));
    private static final MethodHandle SCAN_COUNT_EDGES = h("sekejap_scan_count_edges", FunctionDescriptor.of(C_LONG, PTR));

    // 4.7 transactions
    private static final MethodHandle TX_BEGIN = h("sekejap_tx_begin", FunctionDescriptor.of(PTR, PTR));
    private static final MethodHandle TX_PUT = h("sekejap_tx_put", FunctionDescriptor.of(I32, PTR, PTR, PTR, PTR));
    private static final MethodHandle TX_DELETE = h("sekejap_tx_delete", FunctionDescriptor.of(I32, PTR, PTR, PTR));
    private static final MethodHandle TX_LINK = h("sekejap_tx_link", FunctionDescriptor.of(I32, PTR, PTR, PTR, PTR, PTR, PTR));
    private static final MethodHandle TX_EXECUTE = h("sekejap_tx_execute", FunctionDescriptor.of(C_LONG, PTR, PTR, PTR));
    private static final MethodHandle TX_COMMIT = h("sekejap_tx_commit", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle TX_ROLLBACK = h("sekejap_tx_rollback", FunctionDescriptor.of(I32, PTR));

    // 4.8 maintenance
    private static final MethodHandle CHECKPOINT = h("sekejap_checkpoint", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle PUBLISH = h("sekejap_publish", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle STORAGE = h("sekejap_storage", FunctionDescriptor.of(PTR, PTR));

    // 4.9 service mode
    private static final MethodHandle STATEMENT_TIMEOUT_MS = h("sekejap_statement_timeout_ms", FunctionDescriptor.of(I32, PTR, I64));
    private static final MethodHandle CANCEL = h("sekejap_cancel", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle CLEAR_INTERRUPT = h("sekejap_clear_interrupt", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle SUBSCRIBE = h("sekejap_subscribe", FunctionDescriptor.of(C_LONG, PTR));
    private static final MethodHandle NEXT_CHANGE = h("sekejap_next_change", FunctionDescriptor.of(PTR, PTR, C_LONG, I64));
    private static final MethodHandle UNSUBSCRIBE = h("sekejap_unsubscribe", FunctionDescriptor.of(I32, PTR, C_LONG));

    // 4.10 refused by name
    private static final MethodHandle OPEN_MEMORY = h("sekejap_open_memory", FunctionDescriptor.of(PTR));
    private static final MethodHandle TRIM_MEMORY = h("sekejap_trim_memory", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle COMPACT = h("sekejap_compact", FunctionDescriptor.of(I32, PTR));
    private static final MethodHandle SHOW = h("sekejap_show", FunctionDescriptor.of(PTR, PTR, PTR));

    // ── plumbing ─────────────────────────────────────────────────────────────

    private static MemorySegment seg(long addr) {
        return MemorySegment.ofAddress(addr);
    }

    /** A borrowed C string, or NULL for a Java null. */
    private static MemorySegment str(Arena a, String s) {
        return s == null ? MemorySegment.NULL : a.allocateFrom(s);
    }

    /** Read a library-owned C string and free it once; null for a NULL pointer. */
    private static String takeString(MemorySegment p) throws Throwable {
        if (p.address() == 0) return null;
        String s = p.reinterpret(Long.MAX_VALUE).getString(0);
        STRING_FREE.invokeExact(p);
        return s;
    }

    private static RuntimeException wrap(Throwable e) {
        return (e instanceof RuntimeException) ? (RuntimeException) e : new RuntimeException(e);
    }

    // ── 4.1 opening and identity ─────────────────────────────────────────────

    static long open(String path) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment db = (MemorySegment) OPEN.invokeExact(str(a, path));
            return db.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static long openWithConfig(String path, String configJson) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment db = (MemorySegment) OPEN_WITH_CONFIG.invokeExact(str(a, path), str(a, configJson));
            return db.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static long openService(String path) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment db = (MemorySegment) OPEN_SERVICE.invokeExact(str(a, path));
            return db.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static void close(long db) {
        try { CLOSE.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static String version() {
        try {
            MemorySegment out = (MemorySegment) VERSION.invokeExact();
            return out.reinterpret(Long.MAX_VALUE).getString(0); // static: never freed
        } catch (Throwable e) { throw wrap(e); }
    }

    static int formatVersion() {
        try { return (int) FORMAT_VERSION.invokeExact(); } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.2 errors ───────────────────────────────────────────────────────────

    /**
     * The message for the last failure ON THIS THREAD, or null after a success.
     * The ABI accepts a handle and ignores it -- the slot is thread-local, which
     * is what lets a failed open with no handle still report -- so NULL is passed.
     */
    static String lastError() {
        try {
            MemorySegment out = (MemorySegment) LAST_ERROR.invokeExact(MemorySegment.NULL);
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    /** The code for the last failure on this thread; 0 after a success or a clean miss. */
    static int lastErrorCode() {
        try { return (int) LAST_ERROR_CODE.invokeExact(MemorySegment.NULL); } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.3 documents ────────────────────────────────────────────────────────

    static int put(long db, String collection, String key, String documentJson) {
        try (Arena a = Arena.ofConfined()) {
            return (int) PUT.invokeExact(seg(db), str(a, collection), str(a, key), str(a, documentJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static long putMany(long db, String collection, String rowsJson) {
        try (Arena a = Arena.ofConfined()) {
            if (C_LONG_IS_32) return (int) PUT_MANY.invokeExact(seg(db), str(a, collection), str(a, rowsJson));
            return (long) PUT_MANY.invokeExact(seg(db), str(a, collection), str(a, rowsJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String get(long db, String collection, String key) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) GET.invokeExact(seg(db), str(a, collection), str(a, key));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static int exists(long db, String collection, String key) {
        try (Arena a = Arena.ofConfined()) {
            return (int) EXISTS.invokeExact(seg(db), str(a, collection), str(a, key));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int delete(long db, String collection, String key) {
        try (Arena a = Arena.ofConfined()) {
            return (int) DELETE.invokeExact(seg(db), str(a, collection), str(a, key));
        } catch (Throwable e) { throw wrap(e); }
    }

    static long scanOpen(long db, String collection, long pageRows) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = (MemorySegment) SCAN_OPEN.invokeExact(seg(db), str(a, collection), pageRows);
            return s.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static String scanNext(long scan) {
        try {
            MemorySegment out = (MemorySegment) SCAN_NEXT.invokeExact(seg(scan));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static void scanClose(long scan) {
        try { SCAN_CLOSE.invokeExact(seg(scan)); } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.4 SQL ──────────────────────────────────────────────────────────────

    static long execute(long db, String sql, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            if (C_LONG_IS_32) return (int) EXECUTE.invokeExact(seg(db), str(a, sql), str(a, paramsJson));
            return (long) EXECUTE.invokeExact(seg(db), str(a, sql), str(a, paramsJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String query(long db, String sql, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) QUERY.invokeExact(seg(db), str(a, sql), str(a, paramsJson));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static String explain(long db, String sql, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) EXPLAIN.invokeExact(seg(db), str(a, sql), str(a, paramsJson));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static long prepare(long db, String sql) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = (MemorySegment) PREPARE.invokeExact(seg(db), str(a, sql));
            return s.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static String stmtQuery(long stmt, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) STMT_QUERY.invokeExact(seg(stmt), str(a, paramsJson));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static long stmtExecute(long stmt, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            if (C_LONG_IS_32) return (int) STMT_EXECUTE.invokeExact(seg(stmt), str(a, paramsJson));
            return (long) STMT_EXECUTE.invokeExact(seg(stmt), str(a, paramsJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int stmtRebindable(long stmt) {
        try { return (int) STMT_REBINDABLE.invokeExact(seg(stmt)); } catch (Throwable e) { throw wrap(e); }
    }

    static void stmtFree(long stmt) {
        try { STMT_FREE.invokeExact(seg(stmt)); } catch (Throwable e) { throw wrap(e); }
    }

    static long queryOpen(long db, String sql, String paramsJson, long pageRows) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment s = (MemorySegment) QUERY_OPEN.invokeExact(
                seg(db), str(a, sql), str(a, paramsJson), pageRows);
            return s.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static String queryNext(long scan) {
        try {
            MemorySegment out = (MemorySegment) QUERY_NEXT.invokeExact(seg(scan));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static void queryClose(long scan) {
        try { QUERY_CLOSE.invokeExact(seg(scan)); } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.5 edges ────────────────────────────────────────────────────────────

    static int link(long db, String fromCollection, String fromKey, String edgeType,
                    String toCollection, String toKey) {
        try (Arena a = Arena.ofConfined()) {
            return (int) LINK.invokeExact(seg(db), str(a, fromCollection), str(a, fromKey),
                str(a, edgeType), str(a, toCollection), str(a, toKey));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int linkWith(long db, String fromCollection, String fromKey, String edgeType,
                        String toCollection, String toKey, String propertiesJson) {
        try (Arena a = Arena.ofConfined()) {
            return (int) LINK_WITH.invokeExact(seg(db), str(a, fromCollection), str(a, fromKey),
                str(a, edgeType), str(a, toCollection), str(a, toKey), str(a, propertiesJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int unlink(long db, String fromCollection, String fromKey, String edgeType,
                      String toCollection, String toKey) {
        try (Arena a = Arena.ofConfined()) {
            return (int) UNLINK.invokeExact(seg(db), str(a, fromCollection), str(a, fromKey),
                str(a, edgeType), str(a, toCollection), str(a, toKey));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String neighbours(long db, String collection, String key, String edgeType,
                             int direction, long limit) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) NEIGHBOURS.invokeExact(seg(db), str(a, collection),
                str(a, key), str(a, edgeType), direction, limit);
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.6 the catalog ──────────────────────────────────────────────────────

    static int createCollection(long db, String name, String fieldsJson) {
        try (Arena a = Arena.ofConfined()) {
            return (int) CREATE_COLLECTION.invokeExact(seg(db), str(a, name), str(a, fieldsJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int dropCollection(long db, String name) {
        try (Arena a = Arena.ofConfined()) {
            return (int) DROP_COLLECTION.invokeExact(seg(db), str(a, name));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String collections(long db) {
        try {
            MemorySegment out = (MemorySegment) COLLECTIONS.invokeExact(seg(db));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static String describe(long db, String collection) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) DESCRIBE.invokeExact(seg(db), str(a, collection));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static long countRows(long db, String collection) {
        try (Arena a = Arena.ofConfined()) {
            if (C_LONG_IS_32) return (int) COUNT_ROWS.invokeExact(seg(db), str(a, collection));
            return (long) COUNT_ROWS.invokeExact(seg(db), str(a, collection));
        } catch (Throwable e) { throw wrap(e); }
    }

    static long scanCountRows(long db, String collection) {
        try (Arena a = Arena.ofConfined()) {
            if (C_LONG_IS_32) return (int) SCAN_COUNT_ROWS.invokeExact(seg(db), str(a, collection));
            return (long) SCAN_COUNT_ROWS.invokeExact(seg(db), str(a, collection));
        } catch (Throwable e) { throw wrap(e); }
    }

    static long scanCountEdges(long db) {
        try {
            if (C_LONG_IS_32) return (int) SCAN_COUNT_EDGES.invokeExact(seg(db));
            return (long) SCAN_COUNT_EDGES.invokeExact(seg(db));
        } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.7 transactions ─────────────────────────────────────────────────────

    static long txBegin(long db) {
        try {
            MemorySegment t = (MemorySegment) TX_BEGIN.invokeExact(seg(db));
            return t.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static int txPut(long tx, String collection, String key, String documentJson) {
        try (Arena a = Arena.ofConfined()) {
            return (int) TX_PUT.invokeExact(seg(tx), str(a, collection), str(a, key), str(a, documentJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int txDelete(long tx, String collection, String key) {
        try (Arena a = Arena.ofConfined()) {
            return (int) TX_DELETE.invokeExact(seg(tx), str(a, collection), str(a, key));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int txLink(long tx, String fromCollection, String fromKey, String edgeType,
                      String toCollection, String toKey) {
        try (Arena a = Arena.ofConfined()) {
            return (int) TX_LINK.invokeExact(seg(tx), str(a, fromCollection), str(a, fromKey),
                str(a, edgeType), str(a, toCollection), str(a, toKey));
        } catch (Throwable e) { throw wrap(e); }
    }

    static long txExecute(long tx, String sql, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            if (C_LONG_IS_32) return (int) TX_EXECUTE.invokeExact(seg(tx), str(a, sql), str(a, paramsJson));
            return (long) TX_EXECUTE.invokeExact(seg(tx), str(a, sql), str(a, paramsJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int txCommit(long tx) {
        try { return (int) TX_COMMIT.invokeExact(seg(tx)); } catch (Throwable e) { throw wrap(e); }
    }

    static int txRollback(long tx) {
        try { return (int) TX_ROLLBACK.invokeExact(seg(tx)); } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.8 maintenance ──────────────────────────────────────────────────────

    static int checkpoint(long db) {
        try { return (int) CHECKPOINT.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static int publish(long db) {
        try { return (int) PUBLISH.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static String storage(long db) {
        try {
            MemorySegment out = (MemorySegment) STORAGE.invokeExact(seg(db));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.9 service mode ─────────────────────────────────────────────────────

    static int statementTimeoutMs(long db, long milliseconds) {
        try { return (int) STATEMENT_TIMEOUT_MS.invokeExact(seg(db), milliseconds); }
        catch (Throwable e) { throw wrap(e); }
    }

    static int cancel(long db) {
        try { return (int) CANCEL.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static int clearInterrupt(long db) {
        try { return (int) CLEAR_INTERRUPT.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static long subscribe(long db) {
        try {
            if (C_LONG_IS_32) return (int) SUBSCRIBE.invokeExact(seg(db));
            return (long) SUBSCRIBE.invokeExact(seg(db));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String nextChange(long db, long subscription, long timeoutMs) {
        try {
            MemorySegment out = C_LONG_IS_32
                ? (MemorySegment) NEXT_CHANGE.invokeExact(seg(db), (int) subscription, timeoutMs)
                : (MemorySegment) NEXT_CHANGE.invokeExact(seg(db), subscription, timeoutMs);
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static int unsubscribe(long db, long subscription) {
        try {
            if (C_LONG_IS_32) return (int) UNSUBSCRIBE.invokeExact(seg(db), (int) subscription);
            return (int) UNSUBSCRIBE.invokeExact(seg(db), subscription);
        } catch (Throwable e) { throw wrap(e); }
    }

    // ── 4.10 refused by name ─────────────────────────────────────────────────

    static long openMemory() {
        try {
            MemorySegment db = (MemorySegment) OPEN_MEMORY.invokeExact();
            return db.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static int trimMemory(long db) {
        try { return (int) TRIM_MEMORY.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static int compact(long db) {
        try { return (int) COMPACT.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static String show(long db, String statement) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) SHOW.invokeExact(seg(db), str(a, statement));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }
}
