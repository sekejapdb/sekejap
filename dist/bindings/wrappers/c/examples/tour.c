/*
 * tour.c -- the sekejap C ABI in five stops.
 *
 *   1. documents          put, get, put_many, the paged scan
 *   2. SQL                execute and query with $n, a prepared statement
 *   3. the graph          link, neighbours, unlink
 *   4. transactions       one that commits and one that rolls back
 *   5. the catalog        collections, describe, the three counts
 *
 * Header: dist/ffi/include/sekejap.h. Contract: docs/dist/C_ABI.md.
 * Build and run with `make tour`. A failed check is a non-zero exit.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>   /* mkdtemp */

#include "sekejap.h"

static int failures = 0;

static void check(const char *what, int ok) {
    if (ok) {
        printf("  ok    %s\n", what);
        return;
    }
    failures++;
    char *message = sekejap_last_error(NULL);
    printf("  FAIL  %s (code %d: %s)\n", what,
           (int)sekejap_last_error_code(NULL),
           message ? message : "no message");
    sekejap_string_free(message);
}

int main(void) {
    char directory[] = "/tmp/sekejap-tour-XXXXXX";
    if (mkdtemp(directory) == NULL) {
        perror("mkdtemp");
        return 2;
    }
    SekejapDb *db = sekejap_open(directory);
    check("open", db != NULL);
    if (db == NULL) {
        return 2;
    }
    printf("sekejap %s, disk format %d, in %s\n\n",
           sekejap_version(), sekejap_format_version(), directory);

    /* ── 1. documents ─────────────────────────────────────────────────── */
    puts("1. documents");
    check("declare the collection",
          sekejap_create_collection(
              db, "city",
              "[{\"name\":\"name\",\"kind\":\"text\"},"
              " {\"name\":\"people\",\"kind\":\"int\"}]") == 1);
    check("put one",
          sekejap_put(db, "city", "melbourne",
                      "{\"name\":\"Melbourne\",\"people\":5207000}") == 0);
    check("put many in one commit",
          sekejap_put_many(
              db, "city",
              "[{\"key\":\"sydney\",\"doc\":{\"name\":\"Sydney\",\"people\":5450000}},"
              " {\"key\":\"perth\",\"doc\":{\"name\":\"Perth\",\"people\":2192000}}]") == 2);

    char *one = sekejap_get(db, "city", "perth");
    check("get answers the document with _key set",
          one != NULL && strstr(one, "\"_key\":\"perth\"") != NULL);
    sekejap_string_free(one);
    check("a miss is NULL with the status left ok",
          sekejap_get(db, "city", "hobart") == NULL
              && sekejap_last_error_code(db) == SekejapStatus_Ok);

    /* The walk holds at most two rows at a time; it does not build a list
     * of the collection. */
    SekejapScan *scan = sekejap_scan_open(db, "city", 2);
    check("scan_open", scan != NULL);
    long walked = 0, pages = 0;
    for (;;) {
        char *page = sekejap_scan_next(scan);
        if (page == NULL) {
            break;    /* the end of the walk, with the status left ok */
        }
        pages++;
        for (const char *at = page; (at = strstr(at, "\"_key\"")) != NULL; at += 6) {
            walked++;
        }
        sekejap_string_free(page);
    }
    sekejap_scan_close(scan);
    printf("        %ld rows over %ld pages of at most 2\n", walked, pages);
    check("the walk saw every row", walked == 3 && pages == 2);

    /* ── 2. SQL ───────────────────────────────────────────────────────── */
    puts("\n2. SQL");
    check("an index for the predicate to name",
          sekejap_execute(db, "CREATE INDEX city_people ON city USING btree(people)",
                          NULL) == 0);
    char *rows = sekejap_query(
        db, "SELECT _key, name FROM city WHERE people > $1 ORDER BY people",
        "[3000000]");
    printf("        %s\n", rows ? rows : "(null)");
    check("query answers a JSON array of objects keyed by column name",
          rows != NULL && strstr(rows, "\"name\":\"Melbourne\"") != NULL);
    sekejap_string_free(rows);

    SekejapStmt *stmt = sekejap_prepare(
        db, "SELECT _key FROM city WHERE people > $1");
    check("prepare parses now and compiles on the first bind",
          stmt != NULL
              && sekejap_stmt_rebindable(stmt) == SEKEJAP_REBIND_UNBOUND);
    char *first = sekejap_stmt_query(stmt, "[5000000]");
    check("the first bind compiles and answers", first != NULL);
    sekejap_string_free(first);
    check("and every bind after it is a rebind",
          sekejap_stmt_rebindable(stmt) == 1);
    char *again = sekejap_stmt_query(stmt, "[1000000]");
    check("the rebind answers the other parameter", again != NULL);
    sekejap_string_free(again);
    sekejap_stmt_free(stmt);

    /* ── 3. the graph ─────────────────────────────────────────────────── */
    puts("\n3. the graph");
    check("link",
          sekejap_link(db, "city", "melbourne", "flies_to", "city", "sydney") == 0);
    check("link with properties",
          sekejap_link_with(db, "city", "melbourne", "flies_to", "city", "perth",
                            "{\"hours\":4}") == 0);
    char *out = sekejap_neighbours(db, "city", "melbourne", "flies_to",
                                   SekejapDirection_Outgoing, 16);
    printf("        %s\n", out ? out : "(null)");
    check("the neighbour answer names the collection and the key",
          out != NULL && strstr(out, "\"key\":\"sydney\"") != NULL);
    sekejap_string_free(out);
    check("unlink says whether the edge was there",
          sekejap_unlink(db, "city", "melbourne", "flies_to", "city", "sydney") == 1
              && sekejap_unlink(db, "city", "melbourne", "flies_to", "city", "sydney") == 0);

    /* ── 4. transactions ──────────────────────────────────────────────── */
    puts("\n4. transactions");
    SekejapTx *tx = sekejap_tx_begin(db);
    check("tx_begin", tx != NULL);
    check("two writes under one barrier",
          sekejap_tx_put(tx, "city", "darwin",
                         "{\"name\":\"Darwin\",\"people\":147000}") == 0
              && sekejap_tx_put(tx, "city", "hobart",
                                "{\"name\":\"Hobart\",\"people\":252000}") == 0);
    check("commit", sekejap_tx_commit(tx) == 0);
    check("both writes are there", sekejap_count_rows(db, "city") == 5);

    tx = sekejap_tx_begin(db);
    check("a write and a delete that will not happen",
          sekejap_tx_delete(tx, "city", "darwin") == 1
              && sekejap_tx_put(tx, "city", "cairns",
                                "{\"name\":\"Cairns\",\"people\":153000}") == 0);
    check("rollback", sekejap_tx_rollback(tx) == 0);
    check("neither the delete nor the write survived",
          sekejap_count_rows(db, "city") == 5
              && sekejap_get(db, "city", "cairns") == NULL);

    /* ── 5. the catalog ───────────────────────────────────────────────── */
    puts("\n5. the catalog");
    char *names = sekejap_collections(db);
    printf("        collections: %s\n", names ? names : "(null)");
    check("collections", names != NULL && strstr(names, "city") != NULL);
    sekejap_string_free(names);

    char *shape = sekejap_describe(db, "city");
    printf("        describe:    %s\n", shape ? shape : "(null)");
    check("describe answers _key first",
          shape != NULL && strstr(shape, "\"_key\"") != NULL);
    sekejap_string_free(shape);

    printf("        rows: %ld (record) / %ld (walk), edges: %ld (walk)\n",
           sekejap_count_rows(db, "city"),
           sekejap_scan_count_rows(db, "city"),
           sekejap_scan_count_edges(db));
    check("the record and the walk agree",
          sekejap_count_rows(db, "city") == sekejap_scan_count_rows(db, "city"));

    char *storage = sekejap_storage(db);
    printf("        storage:     %s\n", storage ? storage : "(null)");
    check("storage", storage != NULL);
    sekejap_string_free(storage);

    sekejap_close(db);
    printf(failures == 0 ? "\ntour: every check passed\n"
                         : "\ntour: %d check(s) failed\n", failures);
    return failures == 0 ? 0 : 1;
}
