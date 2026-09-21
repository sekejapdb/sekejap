/*
 * test.c — a plain C program that exercises the sekejap C ABI and asserts.
 *
 * This is how you'd verify the library from C: include the header, link the
 * shared library, and check behavior with assert(). Build & run with `make test`
 * in this directory (or see the commands in the Makefile).
 */

#include "sekejap.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>   /* mkdtemp */

int main(void) {
    /* Use a fresh temp directory so the test is repeatable. */
    char dir[] = "/tmp/sekejap_ctest_XXXXXX";
    if (!mkdtemp(dir)) {
        perror("mkdtemp");
        return 1;
    }

    SekejapDb *db = sekejap_open(dir);
    assert(db != NULL && "open failed");

    /* DDL + SQL insert. */
    assert(sekejap_execute(db, "CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)") >= 0);
    assert(sekejap_execute(db, "INSERT INTO t (_key, v) VALUES ('a', 42)") == 1);

    /* Direct node insert (no SQL). */
    assert(sekejap_put(db, "t/b", "{\"_collection\":\"t\",\"_key\":\"b\",\"v\":7}") == 0);
    assert(sekejap_node_count(db) == 2);
    assert(sekejap_contains(db, "t/a") == 1);
    assert(sekejap_contains(db, "t/zzz") == 0);

    /* Query → JSON array string (caller frees). */
    char *rows = sekejap_query(db, "SELECT v FROM t WHERE _key = 'a'");
    assert(rows != NULL);
    assert(strstr(rows, "42") != NULL);
    sekejap_string_free(rows);

    /* Parameterized query — injection-safe ($1 bound from a JSON array). */
    char *r2 = sekejap_query_params(db, "SELECT _key FROM t WHERE v = $1", "[7]");
    assert(r2 != NULL && strstr(r2, "b") != NULL);
    sekejap_string_free(r2);

    /* An edge, then count it. */
    assert(sekejap_link(db, "t/a", "t/b", "near") == 0);
    assert(sekejap_edge_count(db) == 1);

    /* Error path: a bad statement returns -1 and sets last_error. */
    assert(sekejap_execute(db, "SELECT bad syntax FROM") == -1);
    char *err = sekejap_last_error(db);
    assert(err != NULL);
    printf("  (expected error surfaced: %s)\n", err);
    sekejap_string_free(err);

    sekejap_close(db);
    printf("OK: all sekejap C ABI assertions passed (%s)\n", sekejap_version());
    return 0;
}
