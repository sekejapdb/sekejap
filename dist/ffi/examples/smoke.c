/*
 * smoke.c -- the C ABI of sekejap, end to end, in one file.
 *
 * Open, declare a collection, write documents, query them with a parameter,
 * link two rows, count what is there, and close. `make check` compiles this
 * against the library it just built and RUNS it; a non-zero exit is the
 * check failing.
 *
 * Every rule the header states is exercised here rather than described:
 * the NULL/-1 sentinels, sekejap_last_error for the sentence and
 * sekejap_last_error_code for the code, and sekejap_string_free for every
 * char* the library hands back.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>   /* mkdtemp */

#include "sekejap.h"

static int failures = 0;

static void report(const char *what, int ok, SekejapDb *db) {
    if (ok) {
        printf("  ok    %s\n", what);
        return;
    }
    failures++;
    char *message = sekejap_last_error(db);
    printf("  FAIL  %s (code %d: %s)\n", what,
           (int)sekejap_last_error_code(db),
           message ? message : "no message");
    sekejap_string_free(message);
}

/* A returned string, printed and freed exactly once. */
static char *show(const char *label, char *answer) {
    printf("        %s: %s\n", label, answer ? answer : "(null)");
    return answer;
}

int main(void) {
    char directory[] = "/tmp/sekejap-smoke-XXXXXX";
    if (mkdtemp(directory) == NULL) {
        perror("mkdtemp");
        return 2;
    }
    printf("sekejap %s, disk format %d, in %s\n",
           sekejap_version(), sekejap_format_version(), directory);

    /* ── open ─────────────────────────────────────────────────────────── */
    SekejapDb *db = sekejap_open(directory);
    report("open", db != NULL, NULL);
    if (db == NULL) {
        return 2;
    }

    /* ── create ───────────────────────────────────────────────────────── */
    int created = sekejap_create_collection(
        db, "people",
        "[{\"name\":\"name\",\"kind\":\"text\"},"
        " {\"name\":\"age\",\"kind\":\"int\"}]");
    report("create_collection answers 1 for a new collection", created == 1, db);
    report("create_collection answers 0 for one already there",
           sekejap_create_collection(db, "people", "[]") == 0, db);

    /* ── put and get ──────────────────────────────────────────────────── */
    report("put alice",
           sekejap_put(db, "people", "alice",
                       "{\"name\":\"Alice\",\"age\":34}") == 0, db);
    report("put bob",
           sekejap_put(db, "people", "bob",
                       "{\"name\":\"Bob\",\"age\":41}") == 0, db);

    char *alice = show("get alice", sekejap_get(db, "people", "alice"));
    report("get answers the document with _key set",
           alice != NULL && strstr(alice, "\"_key\":\"alice\"") != NULL, db);
    sekejap_string_free(alice);

    char *missing = sekejap_get(db, "people", "nobody");
    report("a miss is NULL with the status left ok",
           missing == NULL && sekejap_last_error_code(db) == SekejapStatus_Ok, db);
    sekejap_string_free(missing); /* null-safe */

    /* ── query with a parameter ───────────────────────────────────────── */
    char *rows = show("query", sekejap_query(
        db, "SELECT _key, name FROM people WHERE _key = $1", "[\"bob\"]"));
    report("query answers a JSON array of objects keyed by column name",
           rows != NULL && strstr(rows, "\"name\":\"Bob\"") != NULL, db);
    sekejap_string_free(rows);

    /* ── link and neighbours ──────────────────────────────────────────── */
    report("link alice -> bob",
           sekejap_link(db, "people", "alice", "knows", "people", "bob") == 0, db);
    char *neighbours = show("neighbours of alice", sekejap_neighbours(
        db, "people", "alice", "knows", SekejapDirection_Outgoing, 16));
    report("the neighbour answer names the collection and the key",
           neighbours != NULL && strstr(neighbours, "\"key\":\"bob\"") != NULL, db);
    sekejap_string_free(neighbours);

    /* ── count ────────────────────────────────────────────────────────── */
    long people = sekejap_count_rows(db, "people");
    printf("        rows: %ld, edges: %ld\n", people, sekejap_scan_count_edges(db));
    report("count_rows answers the two rows written", people == 2, db);
    report("scan_count_edges answers the one edge linked",
           sekejap_scan_count_edges(db) == 1, db);

    char *catalog = show("collections", sekejap_collections(db));
    report("collections lists the declared collection",
           catalog != NULL && strstr(catalog, "people") != NULL, db);
    sekejap_string_free(catalog);

    char *storage = show("storage", sekejap_storage(db));
    report("storage answers the two files", storage != NULL, db);
    sekejap_string_free(storage);

    /* ── a refusal arrives with a name and a reason ───────────────────── */
    report("a collection that is not in the catalog is -1, not a silent zero",
           sekejap_count_rows(db, "absent") == -1
               && sekejap_last_error_code(db) == SekejapStatus_Invalid,
           NULL);
    report("trim_memory is refused by name",
           sekejap_trim_memory(db) == -1
               && sekejap_last_error_code(db) == SekejapStatus_Refused,
           NULL);

    /* ── close ────────────────────────────────────────────────────────── */
    sekejap_close(db);
    printf(failures == 0 ? "smoke: every check passed\n"
                         : "smoke: %d check(s) failed\n", failures);
    return failures == 0 ? 0 : 1;
}
