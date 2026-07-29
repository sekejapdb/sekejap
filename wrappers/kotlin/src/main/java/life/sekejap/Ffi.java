package life.sekejap;

import java.io.File;
import java.io.InputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.AddressLayout;
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
 * FFM (Panama, JDK 22+) layer over the sekejap C ABI. Package-private — the public
 * API is {@link SekejapDB}. Written in Java because Kotlin can't express
 * {@code MethodHandle.invokeExact}'s polymorphic signature.
 *
 * <p>Finds {@code libsekejap} via {@code -Dsekejap.lib=<path>}, or by searching
 * {@code java.library.path} for the platform library name.
 */
final class Ffi {
    private Ffi() {}

    private static final Linker LINKER = Linker.nativeLinker();
    private static final SymbolLookup LOOKUP = loadLibrary();
    private static final AddressLayout ADDR = ValueLayout.ADDRESS;
    private static final ValueLayout.OfLong LONG = ValueLayout.JAVA_LONG;
    private static final ValueLayout.OfInt INT = ValueLayout.JAVA_INT;

    private static SymbolLookup loadLibrary() {
        String name = System.mapLibraryName("sekejap"); // libsekejap.dylib / .so / sekejap.dll

        // 1. Explicit override.
        String explicit = System.getProperty("sekejap.lib");
        if (explicit != null) {
            return SymbolLookup.libraryLookup(explicit, Arena.global());
        }
        // 2. java.library.path (dev builds pointing at target/release).
        String search = System.getProperty("java.library.path", "");
        for (String dir : search.split(File.pathSeparator)) {
            if (dir.isEmpty()) continue;
            File f = new File(dir, name);
            if (f.exists()) {
                return SymbolLookup.libraryLookup(f.getAbsolutePath(), Arena.global());
            }
        }
        // 3. Bundled in the jar under /natives/<os>-<arch>/ — extract, then load.
        String resource = "/natives/" + platformDir() + "/" + name;
        try (InputStream in = Ffi.class.getResourceAsStream(resource)) {
            if (in != null) {
                Path tmp = Files.createTempFile("libsekejap", suffixOf(name));
                tmp.toFile().deleteOnExit();
                Files.copy(in, tmp, StandardCopyOption.REPLACE_EXISTING);
                return SymbolLookup.libraryLookup(tmp.toAbsolutePath().toString(), Arena.global());
            }
        } catch (Exception e) {
            throw new RuntimeException("failed to extract bundled native " + resource, e);
        }
        throw new RuntimeException(
            "libsekejap not found — set -Dsekejap.lib=<path/to/" + name + ">, add its dir to "
            + "-Djava.library.path, or use a jar built with bundled natives (" + resource + ")");
    }

    /** OS+arch key matching the CI-produced resource dirs, e.g. "macos-aarch64". */
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
            LOOKUP.find(name).orElseThrow(() -> new RuntimeException("symbol not found: " + name)), d);
    }

    private static final MethodHandle OPEN = h("sekejap_open", FunctionDescriptor.of(ADDR, ADDR));
    private static final MethodHandle OPEN_PAGED = h("sekejap_open_paged", FunctionDescriptor.of(ADDR, ADDR));
    private static final MethodHandle CLOSE = h("sekejap_close", FunctionDescriptor.ofVoid(ADDR));
    private static final MethodHandle EXECUTE = h("sekejap_execute", FunctionDescriptor.of(LONG, ADDR, ADDR));
    private static final MethodHandle EXECUTE_PARAMS = h("sekejap_execute_params", FunctionDescriptor.of(LONG, ADDR, ADDR, ADDR));
    private static final MethodHandle QUERY = h("sekejap_query", FunctionDescriptor.of(ADDR, ADDR, ADDR));
    private static final MethodHandle QUERY_PARAMS = h("sekejap_query_params", FunctionDescriptor.of(ADDR, ADDR, ADDR, ADDR));
    private static final MethodHandle PUT = h("sekejap_put", FunctionDescriptor.of(INT, ADDR, ADDR, ADDR));
    private static final MethodHandle GET = h("sekejap_get", FunctionDescriptor.of(ADDR, ADDR, ADDR));
    private static final MethodHandle LINK = h("sekejap_link", FunctionDescriptor.of(INT, ADDR, ADDR, ADDR, ADDR));
    private static final MethodHandle LINK_META = h("sekejap_link_meta", FunctionDescriptor.of(INT, ADDR, ADDR, ADDR, ADDR, ADDR));
    private static final MethodHandle UNLINK = h("sekejap_unlink", FunctionDescriptor.of(INT, ADDR, ADDR, ADDR, ADDR));
    private static final MethodHandle CONTAINS = h("sekejap_contains", FunctionDescriptor.of(INT, ADDR, ADDR));
    private static final MethodHandle NODE_COUNT = h("sekejap_node_count", FunctionDescriptor.of(LONG, ADDR));
    private static final MethodHandle EDGE_COUNT = h("sekejap_edge_count", FunctionDescriptor.of(LONG, ADDR));
    private static final MethodHandle COMPACT = h("sekejap_compact", FunctionDescriptor.of(INT, ADDR));
    private static final MethodHandle PREPARE = h("sekejap_prepare", FunctionDescriptor.of(ADDR, ADDR, ADDR));
    private static final MethodHandle QUERY_PREPARED = h("sekejap_query_prepared", FunctionDescriptor.of(ADDR, ADDR, ADDR, ADDR));
    private static final MethodHandle STMT_FREE = h("sekejap_stmt_free", FunctionDescriptor.ofVoid(ADDR));
    private static final MethodHandle LAST_ERROR = h("sekejap_last_error", FunctionDescriptor.of(ADDR, ADDR));
    private static final MethodHandle STRING_FREE = h("sekejap_string_free", FunctionDescriptor.ofVoid(ADDR));
    private static final MethodHandle VERSION = h("sekejap_version", FunctionDescriptor.of(ADDR));

    private static MemorySegment seg(long addr) {
        return MemorySegment.ofAddress(addr);
    }

    /** Read + free a library-owned C string; null if the pointer is null. */
    private static String takeString(MemorySegment p) throws Throwable {
        if (p.address() == 0) return null;
        String s = p.reinterpret(Long.MAX_VALUE).getString(0);
        STRING_FREE.invokeExact(p);
        return s;
    }

    static long open(String path) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment db = (MemorySegment) OPEN.invokeExact(a.allocateFrom(path));
            return db.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static long openPaged(String path) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment db = (MemorySegment) OPEN_PAGED.invokeExact(a.allocateFrom(path));
            return db.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static void close(long db) {
        try { CLOSE.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static long execute(long db, String sql) {
        try (Arena a = Arena.ofConfined()) {
            return (long) EXECUTE.invokeExact(seg(db), a.allocateFrom(sql));
        } catch (Throwable e) { throw wrap(e); }
    }

    static long executeParams(long db, String sql, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            return (long) EXECUTE_PARAMS.invokeExact(seg(db), a.allocateFrom(sql), a.allocateFrom(paramsJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String query(long db, String sql) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) QUERY.invokeExact(seg(db), a.allocateFrom(sql));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static String queryParams(long db, String sql, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) QUERY_PARAMS.invokeExact(seg(db), a.allocateFrom(sql), a.allocateFrom(paramsJson));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static int put(long db, String slug, String payloadJson) {
        try (Arena a = Arena.ofConfined()) {
            return (int) PUT.invokeExact(seg(db), a.allocateFrom(slug), a.allocateFrom(payloadJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static String get(long db, String slug) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) GET.invokeExact(seg(db), a.allocateFrom(slug));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static int link(long db, String from, String to, String type) {
        try (Arena a = Arena.ofConfined()) {
            return (int) LINK.invokeExact(seg(db), a.allocateFrom(from), a.allocateFrom(to), a.allocateFrom(type));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int linkMeta(long db, String from, String to, String type, String metaJson) {
        try (Arena a = Arena.ofConfined()) {
            return (int) LINK_META.invokeExact(seg(db), a.allocateFrom(from), a.allocateFrom(to), a.allocateFrom(type), a.allocateFrom(metaJson));
        } catch (Throwable e) { throw wrap(e); }
    }

    static int unlink(long db, String from, String to, String type) {
        try (Arena a = Arena.ofConfined()) {
            return (int) UNLINK.invokeExact(seg(db), a.allocateFrom(from), a.allocateFrom(to), a.allocateFrom(type));
        } catch (Throwable e) { throw wrap(e); }
    }

    static boolean contains(long db, String slug) {
        try (Arena a = Arena.ofConfined()) {
            return ((int) CONTAINS.invokeExact(seg(db), a.allocateFrom(slug))) == 1;
        } catch (Throwable e) { throw wrap(e); }
    }

    static long nodeCount(long db) {
        try { return (long) NODE_COUNT.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static long edgeCount(long db) {
        try { return (long) EDGE_COUNT.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static int compact(long db) {
        try { return (int) COMPACT.invokeExact(seg(db)); } catch (Throwable e) { throw wrap(e); }
    }

    static long prepare(long db, String sql) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment stmt = (MemorySegment) PREPARE.invokeExact(seg(db), a.allocateFrom(sql));
            return stmt.address();
        } catch (Throwable e) { throw wrap(e); }
    }

    static String queryPrepared(long db, long stmt, String paramsJson) {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment out = (MemorySegment) QUERY_PREPARED.invokeExact(seg(db), seg(stmt), a.allocateFrom(paramsJson));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static void stmtFree(long stmt) {
        try { STMT_FREE.invokeExact(seg(stmt)); } catch (Throwable e) { throw wrap(e); }
    }

    static String lastError(long db) {
        try {
            MemorySegment out = (MemorySegment) LAST_ERROR.invokeExact(seg(db));
            return takeString(out);
        } catch (Throwable e) { throw wrap(e); }
    }

    static String version() {
        try {
            MemorySegment out = (MemorySegment) VERSION.invokeExact();
            return out.reinterpret(Long.MAX_VALUE).getString(0); // static string — do NOT free
        } catch (Throwable e) { throw wrap(e); }
    }

    private static RuntimeException wrap(Throwable e) {
        return (e instanceof RuntimeException) ? (RuntimeException) e : new RuntimeException(e);
    }
}
