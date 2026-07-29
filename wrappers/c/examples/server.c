/*
 * server.c — the "build your own server" demo: one thread-safe SekejapEngine
 * handle shared by several worker threads (readers) plus a writer, at the same
 * time. This is the shape of an embedded DB behind a multi-threaded service.
 *
 * Requires the `engine` feature. Build & run with `make server`, which builds
 * libsekejap with --features engine and compiles this with -DSEKEJAP_ENGINE.
 */

#include "sekejap.h"

#include <assert.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static SekejapEngine *ENGINE;

/* Each reader thread hammers SELECT concurrently. */
static void *reader(void *arg) {
    (void)arg;
    for (int i = 0; i < 200; i++) {
        char *r = sekejap_engine_query(ENGINE, "SELECT COUNT(*) AS n FROM t");
        assert(r != NULL && "concurrent read must not fail");
        sekejap_string_free(r);
    }
    return NULL;
}

int main(void) {
    ENGINE = sekejap_engine_open_memory();
    assert(ENGINE != NULL);

    assert(sekejap_engine_execute(ENGINE, "CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)") >= 0);
    for (int i = 0; i < 50; i++) {
        char sql[128];
        snprintf(sql, sizeof sql, "INSERT INTO t (_key, v) VALUES ('k%d', %d)", i, i);
        assert(sekejap_engine_execute(ENGINE, sql) == 1);
    }
    sekejap_engine_flush(ENGINE);

    /* 4 readers run while the main thread writes 50 more rows. */
    pthread_t th[4];
    for (int i = 0; i < 4; i++) {
        assert(pthread_create(&th[i], NULL, reader, NULL) == 0);
    }
    for (int i = 50; i < 100; i++) {
        char sql[128];
        snprintf(sql, sizeof sql, "INSERT INTO t (_key, v) VALUES ('k%d', %d)", i, i);
        assert(sekejap_engine_execute(ENGINE, sql) == 1);
    }
    sekejap_engine_flush(ENGINE);
    for (int i = 0; i < 4; i++) {
        pthread_join(th[i], NULL);
    }

    char *rows = sekejap_engine_query(ENGINE, "SELECT COUNT(*) AS n FROM t");
    printf("  final count row: %s\n", rows);
    assert(strstr(rows, "100") != NULL);
    sekejap_string_free(rows);

    sekejap_engine_close(ENGINE);
    printf("OK: concurrent engine server demo passed\n");
    return 0;
}
