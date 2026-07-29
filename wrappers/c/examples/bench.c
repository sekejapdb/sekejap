/* Cross-wrapper micro-benchmark, C over the C ABI. See examples/bench_native.rs. */
#include "sekejap.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    char dir[] = "/tmp/skbench_c_XXXXXX";
    if (!mkdtemp(dir)) return 1;

    SekejapDb *db = sekejap_open(dir);
    sekejap_execute(db, "CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)");
    for (int i = 0; i < 1000; i++) {
        char s[128];
        snprintf(s, sizeof s, "INSERT INTO t (_key, v) VALUES ('k%d', %d)", i, i);
        sekejap_execute(db, s);
    }

    long n = getenv("N") ? atol(getenv("N")) : 50000;
    const char *sql = "SELECT v FROM t WHERE _key = 'k500'";
    char *w = sekejap_query(db, sql);
    sekejap_string_free(w); /* warm */

    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    for (long i = 0; i < n; i++) {
        char *r = sekejap_query(db, sql);
        sekejap_string_free(r);
    }
    clock_gettime(CLOCK_MONOTONIC, &b);

    double el = (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) / 1e9;
    printf("c %.0f %.3f\n", n / el, el * 1e6 / n);
    sekejap_close(db);
    return 0;
}
